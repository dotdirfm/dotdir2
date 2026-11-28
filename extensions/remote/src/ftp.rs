use crate::{Entry, NativeTransfer, ObjectVersion, Profile, RemoteFile, Upload, dal, run};
use anyhow::{Result, ensure};
use dotdir_core::copy::CancelToken;
use std::sync::Arc;
use suppaftp::{
    tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream},
    types::FileType,
};
use tokio::io::AsyncWriteExt;
use url::Url;
use zeroize::Zeroizing;

pub(crate) struct Backend {
    read: dal::Backend,
    address: Url,
    profile: Profile,
    password: Zeroizing<String>,
}

impl Backend {
    pub fn new(profile: &Profile, password: &str) -> Result<Self> {
        Ok(Self {
            read: dal::Backend::ftp(profile, password)?,
            address: Url::parse(&profile.endpoint)?,
            profile: profile.clone(),
            password: Zeroizing::new(password.to_owned()),
        })
    }

    async fn connect(&self) -> Result<AsyncRustlsFtpStream> {
        let host = self.address.host_str().expect("validated FTP host");
        let stream = AsyncRustlsFtpStream::connect((
            host.trim_matches(['[', ']']),
            self.address.port().unwrap_or(21),
        ))
        .await?;
        let mut stream = if self.address.scheme() == "ftps" {
            let mut roots = rustls::RootCertStore::empty();
            roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
            let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
            stream
                .into_secure(AsyncRustlsConnector::from(connector), host)
                .await?
        } else {
            stream
        };
        let (user, password) = if self.profile.user.is_empty() {
            ("anonymous", "anonymous@")
        } else {
            (self.profile.user.as_str(), self.password.as_str())
        };
        stream.login(user, password).await?;
        stream
            .cwd(&format!("/{}", self.profile.root.trim_matches('/')))
            .await?;
        stream.transfer_type(FileType::Binary).await?;
        Ok(stream)
    }
}

impl crate::Backend for Backend {
    fn entries(&self, path: &str, cancel: &CancelToken) -> Result<Vec<Entry>> {
        self.read.entries(path, cancel)
    }
    fn open(&self, path: &str, cancel: &CancelToken) -> Result<RemoteFile> {
        self.read.open(path, cancel)
    }
    fn upload_version(&self, path: &str, cancel: &CancelToken) -> Result<Option<ObjectVersion>> {
        self.read.upload_version(path, cancel)
    }
    fn create_dir(&self, path: &str, cancel: &CancelToken) -> Result<()> {
        self.read.create_dir(path, cancel)
    }

    fn remove(&self, path: &str, expected: &ObjectVersion, cancel: &CancelToken) -> Result<()> {
        run(cancel, async {
            let mut stream = self.connect().await?;
            if expected.directory {
                stream.rmdir(path).await?;
            } else {
                ensure!(
                    self.read.version(path).await?.as_ref() == Some(expected),
                    "FTP source changed before removal"
                );
                stream.rm(path).await?;
            }
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
        if !op.moving || op.source.directory {
            return Ok(false);
        }
        run(cancel, async {
            let mut stream = self.connect().await?;
            ensure!(
                self.read.version(from).await?.as_ref() == Some(op.source),
                "FTP source changed before moving"
            );
            ensure!(
                self.read.version(to).await?.as_ref() == op.previous,
                "FTP destination changed before moving"
            );
            stream.rename(from, to).await?;
            Ok(true)
        })
    }

    fn upload(&self, path: &str, mut upload: Upload<'_>, cancel: &CancelToken) -> Result<()> {
        let staging = crate::staging_path(path);
        let result = (|| -> Result<()> {
            let mut stream = run(cancel, self.connect())?;
            let mut data = run(cancel, async {
                Ok(stream.put_with_stream(&staging).await?)
            })?;
            crate::pump(&mut upload, cancel, |bytes| {
                run(cancel, async { Ok(data.write_all(bytes).await?) })
            })?;
            run(cancel, async {
                stream.finalize_put_stream(data).await?;
                ensure!(
                    self.read.version(path).await?.as_ref() == upload.previous,
                    "FTP destination changed before publishing"
                );
                stream.rename(staging.as_str(), path).await?;
                Ok(())
            })
        })();
        if result.is_err() {
            let _ = run(&CancelToken::new(), async {
                Ok(self.connect().await?.rm(&staging).await?)
            });
        }
        result
    }
}
