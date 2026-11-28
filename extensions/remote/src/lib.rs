use anyhow::{Context, Result, bail, ensure};
use dotdir_core::copy::CancelToken;
use serde::{Deserialize, Serialize};
#[cfg(any(feature = "ftp", feature = "webdav", feature = "s3", feature = "smb"))]
use std::{future::Future, sync::LazyLock, time::Duration};
use std::{
    io::{Read, Seek},
    sync::Arc,
};
use url::Url;
use zeroize::Zeroizing;

#[cfg(any(feature = "ftp", feature = "webdav", feature = "s3"))]
mod dal;
#[cfg(feature = "smb")]
mod samba;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Ftp,
    WebDav,
    Smb,
    S3,
}

impl Protocol {
    pub const ALL: [Self; 4] = [Self::Ftp, Self::WebDav, Self::Smb, Self::S3];

    pub fn scheme(self) -> &'static str {
        match self {
            Self::Ftp => "ftp",
            Self::WebDav => "webdav",
            Self::Smb => "smb",
            Self::S3 => "s3",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ftp => "FTP / FTPS",
            Self::WebDav => "WebDAV",
            Self::Smb => "SMB",
            Self::S3 => "S3",
        }
    }

    pub fn enabled(self) -> bool {
        match self {
            Self::Ftp => cfg!(feature = "ftp"),
            Self::WebDav => cfg!(feature = "webdav"),
            Self::Smb => cfg!(feature = "smb"),
            Self::S3 => cfg!(feature = "s3"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub protocol: Protocol,
    pub name: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub share: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub aws_profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
}

fn component(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
}

impl Profile {
    pub fn new_credential_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.protocol.enabled(),
            "{} support is disabled",
            self.protocol.label()
        );
        ensure!(
            component(&self.name) && !self.name.contains(['@', '#', '?']),
            "Enter a connection name without slashes or URL delimiters"
        );
        let root = self.root.trim_matches('/');
        ensure!(
            root.is_empty() || root.split('/').all(component),
            "Invalid remote root path"
        );

        let schemes: &[&str] = match self.protocol {
            Protocol::Ftp => &["ftps", "ftp"],
            Protocol::WebDav | Protocol::S3 => &["https", "http"],
            Protocol::Smb => &["smb"],
        };
        if self.protocol != Protocol::S3 || !self.endpoint.is_empty() {
            let endpoint = Url::parse(&self.endpoint).context("Enter a complete server URL")?;
            ensure!(
                schemes.contains(&endpoint.scheme()) && endpoint.host().is_some(),
                "Expected a {} server URL",
                schemes.join(" or ")
            );
            ensure!(
                endpoint.username().is_empty()
                    && endpoint.password().is_none()
                    && endpoint.query().is_none()
                    && endpoint.fragment().is_none(),
                "Use the credential fields for sign-in details"
            );
            if matches!(self.protocol, Protocol::Ftp | Protocol::Smb) {
                ensure!(
                    endpoint.path().is_empty() || endpoint.path() == "/",
                    "Enter the remote root or share separately"
                );
            }
        }
        if matches!(self.protocol, Protocol::S3 | Protocol::Smb) {
            ensure!(component(&self.share), "Enter a bucket or share name");
        }
        if self.protocol == Protocol::S3 {
            ensure!(
                self.user.is_empty() == self.credential_id.is_none(),
                "Enter both the S3 access key ID and secret access key, or use an AWS profile"
            );
            ensure!(
                self.endpoint.is_empty() || !self.region.is_empty(),
                "Enter the region for the S3 endpoint"
            );
            ensure!(
                self.endpoint.is_empty() || !self.user.is_empty() || !self.aws_profile.is_empty(),
                "Choose an AWS profile or enter access keys for the custom endpoint"
            );
        } else {
            ensure!(
                self.credential_id.is_none() || !self.user.is_empty(),
                "Enter a username for the saved password"
            );
        }
        Ok(())
    }

    fn credential(&self, namespace: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(
            &format!("dev.dotdir.native.remote.{namespace}"),
            self.credential_id
                .as_deref()
                .context("Missing credential reference")?,
        )
        .context("Accessing the system credential store")
    }

    pub fn save_secret(&self, namespace: &str, secret: &str) -> Result<()> {
        ensure!(!secret.is_empty(), "Enter a password or secret access key");
        self.credential(namespace)?
            .set_password(secret)
            .context("Saving to the system credential store")
    }

    pub fn remove_secret(&self, namespace: &str) -> Result<()> {
        match self.credential(namespace)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("Removing the saved credential"),
        }
    }

    pub fn connect(&self, namespace: &str, cancel: &CancelToken) -> Result<Connection> {
        self.validate()?;
        let secret = if self.credential_id.is_some() {
            Zeroizing::new(
                self.credential(namespace)?
                    .get_password()
                    .context("Reading the saved credential")?,
            )
        } else {
            Zeroizing::new(String::new())
        };
        self.connect_with_secret(&secret, cancel)
    }

    pub fn connect_with_secret(&self, secret: &str, cancel: &CancelToken) -> Result<Connection> {
        self.validate()?;
        #[cfg(any(feature = "webdav", feature = "s3"))]
        {
            static HTTP_SETUP: std::sync::Once = std::sync::Once::new();
            HTTP_SETUP.call_once(|| {
                let _ = rustls::crypto::ring::default_provider().install_default();
                opendal_http_transport_reqwest::install_default();
            });
        }
        let _ = (secret, cancel);
        match self.protocol {
            #[cfg(feature = "ftp")]
            Protocol::Ftp => Ok(Connection(Arc::new(dal::Backend::ftp(self, secret)?))),
            #[cfg(feature = "webdav")]
            Protocol::WebDav => Ok(Connection(Arc::new(dal::Backend::webdav(self, secret)?))),
            #[cfg(feature = "s3")]
            Protocol::S3 => Ok(Connection(Arc::new(dal::Backend::s3(self, secret)?))),
            #[cfg(feature = "smb")]
            Protocol::Smb => Ok(Connection(Arc::new(samba::Backend::connect(
                self, secret, cancel,
            )?))),
            #[allow(unreachable_patterns)]
            _ => bail!("{} support is disabled", self.protocol.label()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub directory: bool,
    pub size: u64,
    pub modified_ms: u64,
}

pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

pub struct RemoteFile {
    pub size: u64,
    pub reader: Box<dyn ReadSeek>,
}

trait Backend: Send + Sync {
    fn entries(&self, path: &str, cancel: &CancelToken) -> Result<Vec<Entry>>;
    fn open(&self, path: &str, cancel: &CancelToken) -> Result<RemoteFile>;
}

#[derive(Clone)]
pub struct Connection(Arc<dyn Backend>);

impl Connection {
    pub fn entries(&self, path: &[String], cancel: &CancelToken) -> Result<Vec<Entry>> {
        ensure!(
            path.iter().all(|part| component(part)),
            "Invalid remote path"
        );
        self.0.entries(&path.join("/"), cancel)
    }

    pub fn open(&self, path: &[String], cancel: &CancelToken) -> Result<RemoteFile> {
        ensure!(
            !path.is_empty() && path.iter().all(|part| component(part)),
            "Invalid remote file"
        );
        self.0.open(&path.join("/"), cancel)
    }
}

#[cfg(any(feature = "ftp", feature = "webdav", feature = "s3", feature = "smb"))]
static RUNTIME: LazyLock<std::io::Result<tokio::runtime::Runtime>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("dotdir-remote")
        .enable_all()
        .build()
});

#[cfg(any(feature = "ftp", feature = "webdav", feature = "s3", feature = "smb"))]
fn run<T>(cancel: &CancelToken, future: impl Future<Output = Result<T>>) -> Result<T> {
    ensure!(!cancel.is_cancelled(), "Remote operation cancelled");
    let runtime = RUNTIME
        .as_ref()
        .map_err(|e| anyhow::anyhow!("Starting remote I/O: {e}"))?;
    runtime.block_on(async {
        tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(30), future) => {
                result.context("Remote server timed out")?
            }
            _ = async {
                while !cancel.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            } => bail!("Remote operation cancelled"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{Profile, Protocol};
    use dotdir_core::copy::CancelToken;
    use std::io::{Read, Seek, SeekFrom};

    #[test]
    #[cfg(feature = "webdav")]
    fn rejects_credentials_in_the_endpoint() {
        let profile: Profile = serde_json::from_str(
            r#"{
            "protocol":"webdav",
            "name":"office",
            "endpoint":"https://user:pw@server.example/dav"
        }"#,
        )
        .unwrap();
        assert!(profile.validate().is_err());
    }

    #[test]
    #[ignore = "requires local protocol servers configured in DOTDIR_TEST_REMOTE_PROFILES"]
    fn reads_and_seeks_on_configured_servers() {
        let profiles: Vec<Profile> = serde_json::from_slice(
            &std::fs::read(std::env::var_os("DOTDIR_TEST_REMOTE_PROFILES").unwrap()).unwrap(),
        )
        .unwrap();
        let secret = std::env::var("DOTDIR_TEST_REMOTE_SECRET").unwrap();
        let names: Vec<_> = profiles.iter().map(|profile| profile.protocol).collect();
        assert!(
            Protocol::ALL
                .into_iter()
                .filter(|protocol| protocol.enabled())
                .all(|protocol| names.contains(&protocol))
        );
        let expected: Vec<_> = (0..2 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
        for profile in profiles
            .into_iter()
            .filter(|profile| profile.protocol.enabled())
        {
            let cancel = CancelToken::new();
            let connection = profile
                .connect_with_secret(&secret, &cancel)
                .unwrap_or_else(|error| panic!("Connecting to {}: {error:#}", profile.name));
            let listing = connection
                .entries(&[], &cancel)
                .unwrap_or_else(|error| panic!("Listing {}: {error:#}", profile.name));
            assert!(
                listing
                    .iter()
                    .any(|entry| entry.name == "folder" && entry.directory),
                "{}",
                profile.name
            );
            let path = ["folder".to_owned(), "sample.bin".to_owned()];
            let mut file = connection
                .open(&path, &cancel)
                .unwrap_or_else(|error| panic!("Opening {}: {error:#}", profile.name));
            assert_eq!(file.size, expected.len() as u64, "{}", profile.name);
            file.reader.seek(SeekFrom::End(-17)).unwrap();
            let mut suffix = [0; 17];
            file.reader
                .read_exact(&mut suffix)
                .unwrap_or_else(|error| panic!("Reading a range from {}: {error:#}", profile.name));
            assert_eq!(suffix, expected[expected.len() - 17..], "{}", profile.name);
            file.reader.rewind().unwrap();
            let mut contents = Vec::new();
            file.reader
                .read_to_end(&mut contents)
                .unwrap_or_else(|error| panic!("Streaming {}: {error:#}", profile.name));
            assert_eq!(contents, expected, "{}", profile.name);
            file.reader.rewind().unwrap();
            cancel.cancel();
            assert!(file.reader.read(&mut suffix).is_err(), "{}", profile.name);
            println!(
                "{} browsing, streaming, seeking, and cancellation passed",
                profile.name
            );
        }
    }
}
