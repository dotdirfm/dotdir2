use crate::{Entry, NativeTransfer, ObjectVersion, Profile, RUNTIME, RemoteFile, Upload, dal, run};
use anyhow::{Context, Result, ensure};
use dotdir_core::copy::CancelToken;
use reqwest::{Client, Method, RequestBuilder};
use url::Url;
use zeroize::Zeroizing;

pub(crate) struct Backend {
    read: dal::Backend,
    client: Client,
    root: Url,
    user: String,
    password: Zeroizing<String>,
}

impl Backend {
    pub fn new(profile: &Profile, password: &str) -> Result<Self> {
        let mut root = Url::parse(&profile.endpoint)?;
        root.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Invalid WebDAV endpoint"))?
            .pop_if_empty()
            .extend(profile.root.split('/').filter(|part| !part.is_empty()));
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(300))
            .build()?;
        Ok(Self {
            read: dal::Backend::webdav(profile, password)?,
            client,
            root,
            user: profile.user.clone(),
            password: Zeroizing::new(password.to_owned()),
        })
    }

    fn url(&self, path: &str) -> Url {
        let mut url = self.root.clone();
        url.path_segments_mut()
            .expect("hierarchical WebDAV URL")
            .pop_if_empty()
            .extend(path.split('/'));
        url
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let request = self.client.request(method, self.url(path));
        if self.user.is_empty() {
            request
        } else {
            request.basic_auth(&self.user, Some(self.password.as_str()))
        }
    }

    async fn transfer_file(
        &self,
        from: &str,
        to: &str,
        moving: bool,
        source: Option<&ObjectVersion>,
        previous: Option<&ObjectVersion>,
    ) -> Result<()> {
        if let Some(source) = source {
            ensure!(
                self.read.version(from).await?.as_ref().is_some_and(
                    |current| current == source || source.directory && current.directory
                ),
                "WebDAV source changed"
            );
        }
        ensure!(
            self.read.version(to).await?.as_ref() == previous,
            "WebDAV destination changed"
        );
        let destination = self.url(to);
        let mut request = self
            .request(
                Method::from_bytes(if moving { b"MOVE" } else { b"COPY" })?,
                from,
            )
            .header("Destination", destination.as_str())
            .header("Overwrite", if previous.is_some() { "T" } else { "F" });
        if let Some(version) = source.filter(|version| !version.etag.is_empty()) {
            request = request.header("If-Match", &version.etag);
        }
        if let Some(version) = previous.filter(|version| !version.etag.is_empty()) {
            request = request.header("If", format!("<{destination}> ([{}])", version.etag));
        }
        let response = request.send().await?;
        ensure!(
            matches!(response.status().as_u16(), 200 | 201 | 204),
            "WebDAV transfer rejected: {}",
            response.status()
        );
        Ok(())
    }
}

impl crate::Backend for Backend {
    fn entries(&self, path: &str, cancel: &CancelToken) -> Result<Vec<Entry>> {
        self.read.entries(path, cancel)
    }
    fn open(&self, path: &str, cancel: &CancelToken) -> Result<RemoteFile> {
        self.read.open(path, cancel)
    }
    fn open_snapshot(
        &self,
        path: &str,
        version: &ObjectVersion,
        cancel: &CancelToken,
    ) -> Result<RemoteFile> {
        self.read.open_snapshot(path, version, cancel)
    }
    fn upload_version(&self, path: &str, cancel: &CancelToken) -> Result<Option<ObjectVersion>> {
        self.read.upload_version(path, cancel)
    }
    fn create_dir(&self, path: &str, cancel: &CancelToken) -> Result<()> {
        self.read.create_dir(path, cancel)
    }

    fn remove(&self, path: &str, expected: &ObjectVersion, cancel: &CancelToken) -> Result<()> {
        run(cancel, async {
            let path = if expected.directory {
                format!("{path}/")
            } else {
                path.to_owned()
            };
            if !expected.directory {
                ensure!(
                    self.read.version(&path).await?.as_ref() == Some(expected),
                    "WebDAV source changed before removal"
                );
            }
            let mut request = self.request(Method::DELETE, &path);
            if !expected.directory && !expected.etag.is_empty() {
                request = request.header("If-Match", &expected.etag);
            }
            let response = request.send().await?;
            ensure!(
                matches!(response.status().as_u16(), 200 | 202 | 204),
                "WebDAV removal rejected: {}",
                response.status()
            );
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
            if !op.moving || op.previous.is_some() {
                return Ok(false);
            }
            return run(cancel, async {
                self.transfer_file(
                    &format!("{from}/"),
                    &format!("{to}/"),
                    true,
                    Some(op.source),
                    None,
                )
                .await?;
                Ok(true)
            });
        }
        run(cancel, async {
            self.transfer_file(from, to, op.moving, Some(op.source), op.previous)
                .await?;
            Ok(true)
        })
    }

    fn upload(&self, path: &str, mut upload: Upload<'_>, cancel: &CancelToken) -> Result<()> {
        let staging = crate::staging_path(path);
        let (send, receive) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
        let body = futures::stream::unfold(receive, |mut rx| async {
            rx.recv()
                .await
                .map(|bytes| (Ok::<_, std::io::Error>(bytes), rx))
        });
        let request = self
            .request(Method::PUT, &staging)
            .header("If-None-Match", "*")
            .header(reqwest::header::CONTENT_LENGTH, upload.size)
            .body(reqwest::Body::wrap_stream(body));
        let runtime = RUNTIME
            .as_ref()
            .map_err(|error| anyhow::anyhow!("Starting remote I/O: {error}"))?;
        let mut task =
            runtime.spawn(async move { request.send().await?.error_for_status().map(|_| ()) });
        let streamed = crate::pump(&mut upload, cancel, |bytes| {
            run(cancel, async {
                send.send(bytes.to_vec())
                    .await
                    .context("WebDAV upload stopped")
            })
        });
        drop(send);
        let result = (|| -> Result<()> {
            run(cancel, async { Ok((&mut task).await??) })?;
            streamed?;
            run(
                cancel,
                self.transfer_file(&staging, path, true, None, upload.previous),
            )
        })();
        if result.is_err() {
            if !task.is_finished() {
                task.abort();
                let _ = run(&CancelToken::new(), async {
                    let _ = (&mut task).await;
                    Ok(())
                });
            }
            let _ = run(&CancelToken::new(), async {
                self.request(Method::DELETE, &staging).send().await?;
                Ok(())
            });
        }
        result
    }
}
