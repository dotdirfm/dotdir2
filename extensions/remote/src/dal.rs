#[cfg(feature = "s3")]
use crate::{EditTarget, NativeTransfer, Upload};
use crate::{Entry, ObjectVersion, Profile, RemoteFile, component, run};
use anyhow::{Result, ensure};
use dotdir_core::copy::CancelToken;
use futures::io::{AsyncReadExt, AsyncSeekExt};
use opendal::{FuturesAsyncReader, Operator};
use std::io::{self, Read, Seek, SeekFrom};
#[cfg(feature = "s3")]
use std::sync::Arc;

pub(crate) struct Backend(pub(crate) Operator);

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

    async fn open_reader(
        &self,
        path: &str,
        size: u64,
        etag: Option<&str>,
        cancel: &CancelToken,
    ) -> Result<RemoteFile> {
        let mut reader = self.0.reader_with(path);
        if let Some(etag) = etag {
            reader = reader.if_match(etag);
        }
        let reader = reader.await?.into_futures_async_read(0..size).await?;
        Ok(RemoteFile {
            size,
            reader: Box::new(Reader {
                reader,
                cancel: cancel.clone(),
            }),
        })
    }

    pub(crate) async fn version(&self, path: &str) -> Result<Option<ObjectVersion>> {
        match self.0.stat(path).await {
            Ok(metadata) => Ok(Some(object_version(&metadata))),
            Err(error) if error.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

fn object_version(metadata: &opendal::Metadata) -> ObjectVersion {
    ObjectVersion {
        etag: strong_etag(metadata).unwrap_or_default().to_owned(),
        modified_ms: metadata
            .last_modified()
            .map_or(0, |t| t.into_inner().as_millisecond().max(0) as u64),
        size: metadata.content_length(),
        directory: metadata.is_dir(),
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
            self.open_reader(path, metadata.content_length(), None, cancel)
                .await
        })
    }

    fn open_snapshot(
        &self,
        path: &str,
        version: &ObjectVersion,
        cancel: &CancelToken,
    ) -> Result<RemoteFile> {
        run(cancel, async {
            ensure!(
                self.version(path).await?.as_ref() == Some(version),
                "Remote source changed: {path}"
            );
            let etag = (!version.etag.is_empty() && self.0.info().capability().read_with_if_match)
                .then_some(version.etag.as_str());
            self.open_reader(path, version.size, etag, cancel).await
        })
    }

    #[cfg(feature = "s3")]
    fn open_document(
        &self,
        path: &str,
        cancel: &CancelToken,
    ) -> Result<(RemoteFile, Option<Arc<dyn EditTarget>>)> {
        if self.0.info().scheme() != "s3" {
            return Ok((self.open(path, cancel)?, None));
        }
        run(cancel, async {
            let metadata = self.0.stat(path).await?;
            ensure!(metadata.is_file(), "Expected an S3 object");
            let etag = strong_etag(&metadata);
            let file = self
                .open_reader(path, metadata.content_length(), etag, cancel)
                .await?;
            let edit = etag.map(|etag| {
                Arc::new(S3Edit {
                    operator: self.0.clone(),
                    path: path.to_owned(),
                    options: opendal::options::WriteOptions {
                        if_match: Some(etag.to_owned()),
                        cache_control: metadata.cache_control().map(str::to_owned),
                        content_type: metadata.content_type().map(str::to_owned),
                        content_disposition: metadata.content_disposition().map(str::to_owned),
                        content_encoding: metadata.content_encoding().map(str::to_owned),
                        user_metadata: metadata.user_metadata().map(|values| {
                            values
                                .into_iter()
                                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                                .collect()
                        }),
                        ..Default::default()
                    },
                }) as Arc<dyn EditTarget>
            });
            Ok((file, edit))
        })
    }

    fn upload_version(&self, path: &str, cancel: &CancelToken) -> Result<Option<ObjectVersion>> {
        run(cancel, async {
            let version = self.version(path).await?;
            if version.is_none() && self.0.info().scheme() == "s3" {
                self.version(&format!("{path}/")).await
            } else {
                Ok(version)
            }
        })
    }

    fn create_dir(&self, path: &str, cancel: &CancelToken) -> Result<()> {
        let directory = format!("{path}/");
        if self.0.info().scheme() == "s3" {
            #[cfg(feature = "s3")]
            return run(cancel, async {
                use opendal::raw::{Service as _, oio::Write as _};
                ensure!(
                    self.version(path).await?.is_none(),
                    "An S3 object already uses the folder name: {path}"
                );
                if self.version(&directory).await?.is_some() {
                    return Ok(());
                }
                let opts = opendal::options::WriteOptions {
                    if_not_exists: true,
                    ..Default::default()
                };
                let mut writer = self.0.service().write(
                    self.0.context(),
                    &directory,
                    opendal::raw::OpWrite::from_options(&self.0.info().capability(), opts)?.0,
                )?;
                match writer.close().await {
                    Err(error)
                        if error.kind() == opendal::ErrorKind::ConditionNotMatch
                            && self.version(&directory).await?.is_some() =>
                    {
                        Ok(())
                    }
                    result => {
                        result?;
                        Ok(())
                    }
                }
            });
        }
        run(cancel, async { Ok(self.0.create_dir(&directory).await?) })
    }

    fn remove(&self, path: &str, expected: &ObjectVersion, cancel: &CancelToken) -> Result<()> {
        run(cancel, async {
            let key = if expected.directory {
                format!("{path}/")
            } else {
                path.to_owned()
            };
            if expected.directory && self.0.info().scheme() == "s3" {
                // Remove only an explicit folder marker. Descendants are separate objects.
                use opendal::raw::Service as _;
                match self
                    .0
                    .service()
                    .stat(self.0.context(), &key, opendal::raw::OpStat::new())
                    .await
                {
                    Ok(_) => self.0.delete(&key).await?,
                    Err(error) if error.kind() == opendal::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                return Ok(());
            }
            ensure!(
                self.version(&key).await?.as_ref().is_some_and(
                    |current| current == expected || expected.directory && current.directory
                ),
                "Source changed before removal: {path}"
            );
            if self.0.info().capability().delete_with_if_match && !expected.etag.is_empty() {
                self.0.delete_with(&key).if_match(&expected.etag).await?;
            } else {
                ensure!(
                    self.0.info().scheme() != "s3",
                    "S3 did not provide an ETag for the source"
                );
                self.0.delete(&key).await?;
            }
            Ok(())
        })
    }

    #[cfg(feature = "s3")]
    fn transfer(
        &self,
        from: &str,
        to: &str,
        op: NativeTransfer<'_>,
        cancel: &CancelToken,
    ) -> Result<bool> {
        if self.0.info().scheme() != "s3" || op.moving || op.source.directory {
            return Ok(false);
        }
        use opendal::raw::{Service as _, oio::Write as _};
        ensure!(
            !op.source.etag.is_empty() && op.previous.is_none_or(|v| !v.etag.is_empty()),
            "Missing S3 ETag for the copy"
        );
        let mut writer = run(cancel, async {
            let metadata = self.0.stat(from).await?;
            ensure!(
                object_version(&metadata) == *op.source,
                "S3 source changed before copying"
            );
            let options = opendal::options::WriteOptions {
                if_match: op.previous.map(|version| version.etag.clone()),
                if_not_exists: op.previous.is_none(),
                cache_control: metadata.cache_control().map(str::to_owned),
                content_type: metadata.content_type().map(str::to_owned),
                content_disposition: metadata.content_disposition().map(str::to_owned),
                content_encoding: metadata.content_encoding().map(str::to_owned),
                user_metadata: metadata.user_metadata().map(|values| {
                    values
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .collect()
                }),
                ..Default::default()
            };
            // Use the native writer directly. The high-level writer can
            // fetch small source ranges and upload them as ordinary bytes.
            Ok(self.0.service().write(
                self.0.context(),
                to,
                opendal::raw::OpWrite::from_options(&self.0.info().capability(), options)?.0,
            )?)
        })?;
        let result = (|| -> Result<bool> {
            const PART_BYTES: u64 = 1024 * 1024 * 1024;
            for start in (0..op.source.size).step_by(PART_BYTES as usize) {
                let length = (op.source.size - start).min(PART_BYTES);
                let (range, args, _) = opendal::options::ReadOptions {
                    range: (start..start + length).into(),
                    if_match: Some(op.source.etag.clone()),
                    ..Default::default()
                }
                .into();
                run(cancel, async {
                    Ok(writer.copy_from(from, args, range).await?)
                })?;
            }
            run(cancel, async { Ok(writer.close().await?) })?;
            Ok(true)
        })();
        if result.is_err() {
            let _ = run(&CancelToken::new(), async { Ok(writer.abort().await?) });
        }
        result
    }

    #[cfg(feature = "s3")]
    fn upload(&self, path: &str, mut upload: Upload<'_>, cancel: &CancelToken) -> Result<()> {
        ensure!(
            self.0.info().scheme() == "s3",
            "This filesystem does not support uploads"
        );
        let options = opendal::options::WriteOptions {
            if_not_exists: upload.previous.is_none(),
            if_match: upload.previous.map(|object| object.etag.clone()),
            chunk: Some(8 * 1024 * 1024),
            ..Default::default()
        };
        ensure!(
            options.if_match.as_deref() != Some(""),
            "S3 did not return an ETag for the destination"
        );
        let result = self.upload_once(path, &mut upload, options.clone(), cancel);
        if let Err(error) = &result
            && error
                .downcast_ref::<opendal::Error>()
                .is_some_and(condition_failed)
            && options.if_match.is_some()
            && let Some(alternate) = run(cancel, async {
                Ok(alternate_write_options(&self.0, path, &options).await)
            })?
        {
            upload.reader.rewind()?;
            return self.upload_once(path, &mut upload, alternate, cancel);
        }
        result
    }
}

#[cfg(feature = "s3")]
impl Backend {
    fn upload_once(
        &self,
        path: &str,
        upload: &mut Upload<'_>,
        options: opendal::options::WriteOptions,
        cancel: &CancelToken,
    ) -> Result<()> {
        let mut writer = run(cancel, async {
            Ok(self.0.writer_options(path, options).await?)
        })?;
        let result = (|| -> Result<()> {
            let mut remaining = upload.size;
            while remaining > 0 {
                let mut bytes = vec![0; (remaining.min(1024 * 1024)) as usize];
                upload.reader.read_exact(&mut bytes)?;
                remaining -= bytes.len() as u64;
                run(cancel, async { Ok(writer.write(bytes).await?) })?;
                (upload.progress)(upload.size - remaining);
            }
            run(cancel, async { Ok(writer.close().await?) })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = run(&CancelToken::new(), async { Ok(writer.abort().await?) });
        }
        result
    }
}

fn strong_etag(metadata: &opendal::Metadata) -> Option<&str> {
    let etag = metadata.etag()?;
    let value = etag.strip_prefix('"')?.strip_suffix('"')?;
    (!value.is_empty() && value.bytes().all(|b| matches!(b, 0x21 | 0x23..=0x7e))).then_some(etag)
}

#[cfg(feature = "s3")]
fn condition_failed(error: &opendal::Error) -> bool {
    error.kind() == opendal::ErrorKind::ConditionNotMatch
}

#[cfg(feature = "s3")]
async fn alternate_write_options(
    operator: &Operator,
    path: &str,
    options: &opendal::options::WriteOptions,
) -> Option<opendal::options::WriteOptions> {
    let original = options.if_match.as_deref()?;
    let latest = operator.stat(path).await.ok()?;
    if strong_etag(&latest)? != original {
        return None;
    }
    bare_write_options(options)
}

#[cfg(feature = "s3")]
fn bare_write_options(
    options: &opendal::options::WriteOptions,
) -> Option<opendal::options::WriteOptions> {
    // Vary only the syntax of the original tag. A new tag or wildcard would bypass the guard.
    let bare = options
        .if_match
        .as_deref()?
        .strip_prefix('"')?
        .strip_suffix('"')?;
    if !bare.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return None;
    }
    let mut options = options.clone();
    options.if_match = Some(bare.to_owned());
    Some(options)
}

#[cfg(feature = "s3")]
struct S3Edit {
    operator: Operator,
    path: String,
    options: opendal::options::WriteOptions,
}

#[cfg(feature = "s3")]
impl EditTarget for S3Edit {
    fn save(&self, bytes: Vec<u8>, cancel: &CancelToken) -> Result<Arc<dyn EditTarget>> {
        run(cancel, async {
            let mut options = self.options.clone();
            let metadata = self.write(opendal::Buffer::from(bytes)).await?;
            let etag = strong_etag(&metadata).ok_or_else(|| {
                anyhow::anyhow!("S3 accepted the save without returning a usable ETag. Your edits are still open. Reopen the object to continue editing.")
            })?;
            options.if_match = Some(etag.to_owned());
            Ok(Arc::new(Self {
                operator: self.operator.clone(),
                path: self.path.clone(),
                options,
            }) as Arc<dyn EditTarget>)
        })
    }
}

#[cfg(feature = "s3")]
impl S3Edit {
    async fn write(&self, bytes: opendal::Buffer) -> Result<opendal::Metadata> {
        use opendal::ErrorKind::{ConditionNotMatch, Conflict, NotFound};
        let mut options = self.options.clone();
        let mut delays = [150, 350, 750].into_iter();
        let mut alternate = bare_write_options(&self.options);
        loop {
            let error = match self
                .operator
                .write_options(&self.path, bytes.clone(), options.clone())
                .await
            {
                Ok(metadata) => return Ok(metadata),
                Err(error) => error,
            };
            let concurrent_write = error.kind() == Conflict && error.is_temporary();
            if !concurrent_write && error.kind() != ConditionNotMatch {
                let message = if error.kind() == NotFound {
                    "The S3 object was removed. Your edits are still open."
                } else {
                    "Could not save the S3 object. Your edits are still open."
                };
                return Err(anyhow::Error::from(error).context(message));
            }

            let delay = concurrent_write.then(|| delays.next()).flatten();
            if let Some(ms) = delay {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
            let latest = match self.operator.stat(&self.path).await {
                Ok(metadata) => metadata,
                Err(stat) if stat.kind() == NotFound => {
                    return Err(anyhow::Error::from(error)
                        .context("The S3 object was removed. Your edits are still open."));
                }
                Err(stat) => {
                    return Err(
                        anyhow::anyhow!("S3 write: {error}; S3 metadata check: {stat}")
                            .context("Could not verify the S3 object. Your edits are still open."),
                    );
                }
            };
            if strong_etag(&latest) != self.options.if_match.as_deref() {
                return Err(anyhow::Error::from(error)
                    .context("The S3 object changed. Your edits are still open."));
            }
            if delay.is_some() {
                continue;
            }
            if let Some(bare) = alternate.take() {
                options = bare;
                continue;
            }
            return Err(anyhow::Error::from(error)
                .context("S3 keeps rejecting the save. Your edits are still open."));
        }
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
