use anyhow::{Context, Result, ensure};
use gpui_kit::{Image, ImageFormat, ScrollHandle};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek},
    path::{Component, Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};
use url::Url;
use zip::ZipArchive;

const REGISTRY: &str = "https://open-vsx.org";
const PAGE_SIZE: usize = 24;
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;
const MAX_UNPACKED: u64 = 1024 * 1024 * 1024;

pub(crate) enum Event {
    Searched(u64, Result<SearchPage>),
    Selected(u64, Result<Box<Detail>>),
    Installed(String, Result<Box<Installed>>),
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    pub namespace: String,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub version: String,
    #[serde(default)]
    pub download_count: u64,
    pub average_rating: Option<f64>,
    #[serde(default)]
    pub pre_release: bool,
    pub target_platform: Option<String>,
    #[serde(default)]
    files: Files,
}

impl Entry {
    pub fn id(&self) -> String {
        format!("{}.{}", self.namespace, self.name).to_ascii_lowercase()
    }

    pub fn title(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    pub fn page_url(&self) -> String {
        format!("{REGISTRY}/extension/{}/{}", self.namespace, self.name)
    }
}

#[derive(Clone, Default, Deserialize)]
struct Files {
    download: Option<String>,
    sha256: Option<String>,
    readme: Option<String>,
    icon: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchPage {
    pub extensions: Vec<Entry>,
    pub total_size: usize,
    #[serde(skip)]
    pub query: Option<String>,
    #[serde(skip)]
    pub offset: usize,
}

pub(crate) struct Detail {
    pub release: Entry,
    pub readme: Option<String>,
    pub icon: Option<Arc<Image>>,
}

pub(crate) struct Installed {
    pub entry: Entry,
    pub root: PathBuf,
    pub color_theme: bool,
    pub icon_theme: bool,
}

#[derive(Default)]
pub(crate) struct Market {
    pub query: String,
    pub page: SearchPage,
    pub installed: BTreeMap<String, Installed>,
    pub installed_only: bool,
    pub selected: Option<Entry>,
    pub detail: Option<Detail>,
    pub search_request: Option<u64>,
    pub detail_request: Option<u64>,
    pub installing: Option<String>,
    pub search_error: Option<String>,
    pub detail_error: Option<String>,
    pub install_error: Option<(String, String)>,
    pub results_scroll: ScrollHandle,
    pub details_scroll: ScrollHandle,
}

pub(crate) fn search(query: &str, offset: usize) -> Result<SearchPage> {
    if offset == 0
        && let Some((namespace, name)) = extension_id(query)
        && let Some(entry) = release(&namespace, &name)?
    {
        return Ok(SearchPage {
            extensions: vec![entry],
            total_size: 1,
            query: Some(query.into()),
            offset,
        });
    }
    let mut url = Url::parse(&format!("{REGISTRY}/api/-/search"))?;
    url.query_pairs_mut()
        .append_pair("size", &PAGE_SIZE.to_string())
        .append_pair("offset", &offset.to_string())
        .append_pair(
            if query.is_empty() { "sortBy" } else { "query" },
            if query.is_empty() {
                "downloadCount"
            } else {
                query
            },
        );
    let mut page: SearchPage = json(url.as_str())?;
    page.extensions
        .retain(|entry| valid_name(&entry.namespace) && valid_name(&entry.name));
    page.query = Some(query.into());
    page.offset = offset;
    Ok(page)
}

pub(crate) fn details(entry: &Entry) -> Result<Detail> {
    let release =
        release(&entry.namespace, &entry.name)?.context("No Open VSX release is available")?;
    let readme = release
        .files
        .readme
        .as_deref()
        .and_then(|url| bytes(url, 512 * 1024).ok())
        .and_then(|data| String::from_utf8(data).ok());
    let icon = release
        .files
        .icon
        .as_deref()
        .and_then(|url| bytes(url, 2 * 1024 * 1024).ok())
        .and_then(|data| {
            let format = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
                ImageFormat::Png
            } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
                ImageFormat::Jpeg
            } else {
                return None;
            };
            Some(Arc::new(Image::from_bytes(format, data)))
        });
    Ok(Detail {
        release,
        readme,
        icon,
    })
}

impl SearchPage {
    pub fn next_offset(&self) -> Option<usize> {
        let next = self.offset + PAGE_SIZE;
        (next < self.total_size).then_some(next)
    }

    pub fn previous_offset(&self) -> Option<usize> {
        self.offset.checked_sub(PAGE_SIZE)
    }
}

fn release(namespace: &str, name: &str) -> Result<Option<Entry>> {
    ensure!(
        valid_name(namespace) && valid_name(name),
        "Invalid extension identifier"
    );
    for platform in target_platform().into_iter().chain(["universal"]) {
        let url = format!("{REGISTRY}/api/{namespace}/{name}/{platform}/latest");
        match response(&url) {
            Ok(mut response) => {
                let data = read_bytes(response.body_mut().as_reader(), 2 * 1024 * 1024)?;
                let entry: Entry = serde_json::from_slice(&data)?;
                if entry
                    .target_platform
                    .as_deref()
                    .is_none_or(|target| target == platform)
                {
                    return Ok(Some(entry));
                }
            }
            Err(ureq::Error::StatusCode(404)) => {}
            Err(error) => return Err(error).context("Open VSX release lookup failed"),
        }
    }
    // Show metadata for an extension published only for other platforms.
    match response(&format!("{REGISTRY}/api/{namespace}/{name}/latest")) {
        Ok(mut result) => Ok(Some(serde_json::from_slice(&read_bytes(
            result.body_mut().as_reader(),
            2 * 1024 * 1024,
        )?)?)),
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        Err(error) => Err(error).context("Open VSX release lookup failed"),
    }
}

fn target_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("windows", "aarch64") => Some("win32-arm64"),
        ("windows", "x86_64") => Some("win32-x64"),
        ("windows", "x86") => Some("win32-ia32"),
        ("linux", "aarch64") if cfg!(target_env = "musl") => Some("alpine-arm64"),
        ("linux", "x86_64") if cfg!(target_env = "musl") => Some("alpine-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "arm") => Some("linux-armhf"),
        _ => None,
    }
}

impl Entry {
    pub fn installable(&self) -> bool {
        self.files.download.is_some()
            && self
                .target_platform
                .as_deref()
                .is_none_or(|target| target == "universal" || Some(target) == target_platform())
    }
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn extension_id(query: &str) -> Option<(String, String)> {
    let (namespace, name) = if let Ok(url) = Url::parse(query)
        && url.origin() == Url::parse(REGISTRY).ok()?.origin()
    {
        let mut segments = url.path_segments()?;
        (segments.next()? == "extension").then_some(())?;
        (segments.next()?.to_owned(), segments.next()?.to_owned())
    } else {
        let (namespace, name) = query.split_once('.')?;
        (namespace.into(), name.into())
    };
    (valid_name(&namespace) && valid_name(&name)).then_some((namespace, name))
}

fn response(url: &str) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::Agent::config_builder()
                .https_only(true)
                .timeout_connect(Some(Duration::from_secs(12)))
                .timeout_per_read(Some(Duration::from_secs(20)))
                .timeout_global(Some(Duration::from_secs(180)))
                .tls_config(
                    ureq::tls::TlsConfig::builder()
                        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                        .build(),
                )
                .build()
                .into()
        })
        .get(url)
        .header("User-Agent", "DotDir/0.1 (Open VSX)")
        .call()
}

fn asset_url(url: &str) -> Result<()> {
    let url = Url::parse(url)?;
    ensure!(
        url.origin() == Url::parse(REGISTRY)?.origin() && url.path().starts_with("/api/"),
        "Unexpected Open VSX asset URL"
    );
    Ok(())
}

fn read_bytes(reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(limit + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= limit,
        "Open VSX response exceeds the size limit"
    );
    Ok(data)
}

fn bytes(url: &str, limit: u64) -> Result<Vec<u8>> {
    asset_url(url)?;
    read_bytes(response(url)?.body_mut().as_reader(), limit)
}

fn json<T: DeserializeOwned>(url: &str) -> Result<T> {
    Ok(serde_json::from_slice(&bytes(url, 4 * 1024 * 1024)?)?)
}

pub(crate) fn install(entry: &Entry, directory: &Path) -> Result<Installed> {
    ensure!(
        entry.installable(),
        "This release is unavailable for this platform"
    );
    ensure!(
        valid_name(&entry.namespace) && valid_name(&entry.name),
        "Invalid extension identifier"
    );
    ensure!(
        !entry.version.is_empty()
            && entry.version.len() <= 128
            && entry
                .version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')),
        "Invalid extension version"
    );
    let platform = entry
        .target_platform
        .as_deref()
        .filter(|target| *target != "universal");
    let suffix = platform.map_or_else(String::new, |target| format!("-{target}"));
    let destination = directory.join(format!("{}-{}{suffix}", entry.id(), entry.version));
    if destination.exists() {
        return check_manifest(&destination.join("extension"), entry);
    }
    fs::create_dir_all(directory)?;
    let staging = tempfile::Builder::new()
        .prefix(".dotdir-vsix-")
        .tempdir_in(directory.parent().context("Missing profile directory")?)?;
    let mut download = tempfile::tempfile_in(staging.path())?;
    let url = entry
        .files
        .download
        .as_deref()
        .context("No downloadable VSIX")?;
    asset_url(url)?;
    let size = io::copy(
        &mut response(url)?.body_mut().as_reader().take(MAX_DOWNLOAD + 1),
        &mut download,
    )?;
    ensure!(size <= MAX_DOWNLOAD, "VSIX exceeds the download size limit");
    download.rewind()?;
    if let Some(url) = &entry.files.sha256 {
        let digest = String::from_utf8(bytes(url, 1024)?)?;
        let mut hash = Sha256::new();
        io::copy(&mut download, &mut hash)?;
        ensure!(
            digest
                .split_whitespace()
                .next()
                .is_some_and(|value| value.eq_ignore_ascii_case(&format!("{:x}", hash.finalize()))),
            "The VSIX checksum did not match Open VSX"
        );
        download.rewind()?;
    }
    let unpacked = staging.path().join("package");
    let root = unpacked.join("extension");
    fs::create_dir_all(&root)?;
    unpack(download, &root)?;
    let mut package = check_manifest(&root, entry)?;
    match fs::rename(&unpacked, &destination) {
        Ok(()) => {
            package.root = destination.join("extension");
            Ok(package)
        }
        Err(_) if destination.is_dir() => check_manifest(&destination.join("extension"), entry),
        Err(error) => Err(error).context("Could not finish installing the extension"),
    }
}

fn unpack(file: File, root: &Path) -> Result<()> {
    let mut archive = ZipArchive::new(file).context("Invalid VSIX archive")?;
    let mut remaining = MAX_UNPACKED;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if !entry.name()?.starts_with("extension/") {
            continue;
        }
        let path = entry.enclosed_name().context("Invalid path in VSIX")?;
        let path = path.strip_prefix("extension")?;
        ensure!(
            path.components().all(|part| {
                matches!(part, Component::Normal(name) if valid_file_name(&name.to_string_lossy()))
            }),
            "Invalid extension file path"
        );
        let destination = root.join(path);
        let mode = entry.unix_mode().unwrap_or(0);
        ensure!(
            matches!(mode & 0o170000, 0 | 0o100000 | 0o040000),
            "Unsupported file type in VSIX"
        );
        if entry.is_dir() {
            fs::create_dir_all(destination)?;
            continue;
        }
        let limit = entry.size();
        ensure!(limit <= remaining, "VSIX exceeds the extraction size limit");
        fs::create_dir_all(
            destination
                .parent()
                .context("Missing extension file parent")?,
        )?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 });
        }
        let written = io::copy(
            &mut (&mut entry).take(limit + 1),
            &mut options.open(destination)?,
        )?;
        ensure!(
            written == limit,
            "VSIX file size did not match its directory entry"
        );
        remaining -= written;
    }
    Ok(())
}

fn valid_file_name(name: &str) -> bool {
    if name.contains(['\\', ':']) || name.ends_with(['.', ' ']) {
        return false;
    }
    if cfg!(windows) {
        let stem = name
            .split('.')
            .next()
            .unwrap_or(name)
            .trim_end()
            .to_ascii_uppercase();
        if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].into_iter().any(|prefix| {
                stem.strip_prefix(prefix).is_some_and(|suffix| {
                    matches!(
                        suffix,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                })
            })
        {
            return false;
        }
    }
    true
}

fn check_manifest(root: &Path, expected: &Entry) -> Result<Installed> {
    let installed = read_installed(root)?;
    ensure!(
        installed.entry.id() == expected.id() && installed.entry.version == expected.version,
        "VSIX package identity differs from the selected Open VSX release"
    );
    Ok(installed)
}

fn read_installed(root: &Path) -> Result<Installed> {
    let manifest: serde_json::Value =
        serde_json::from_reader(File::open(root.join("package.json"))?)?;
    let namespace = manifest["publisher"]
        .as_str()
        .filter(|name| valid_name(name))
        .context("Missing VSIX publisher")?;
    let name = manifest["name"]
        .as_str()
        .filter(|name| valid_name(name))
        .context("Missing VSIX name")?;
    let version = manifest["version"]
        .as_str()
        .context("Missing VSIX version")?;
    let contributes = &manifest["contributes"];
    let has = |name| {
        contributes[name]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    };
    Ok(Installed {
        entry: Entry {
            namespace: namespace.into(),
            name: name.into(),
            version: version.into(),
            display_name: manifest["displayName"].as_str().map(str::to_owned),
            description: manifest["description"].as_str().map(str::to_owned),
            download_count: 0,
            average_rating: None,
            pre_release: false,
            target_platform: None,
            files: Files::default(),
        },
        root: root.to_path_buf(),
        color_theme: has("themes"),
        icon_theme: has("iconThemes"),
    })
}

pub(crate) fn installed(directory: &Path) -> BTreeMap<String, Installed> {
    let mut packages = BTreeMap::<String, Installed>::new();
    for item in fs::read_dir(directory).into_iter().flatten().flatten() {
        let path = item.path();
        let Some(package) = [path.join("extension"), path]
            .into_iter()
            .find_map(|root| read_installed(&root).ok())
        else {
            continue;
        };
        let id = package.entry.id();
        if packages.get(&id).is_none_or(|old| {
            (
                crate::vscode::version_key(&package.entry.version),
                &package.root,
            ) > (crate::vscode::version_key(&old.entry.version), &old.root)
        }) {
            packages.insert(id, package);
        }
    }
    packages
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn accepts_registry_identifiers_and_page_urls() {
        let expected = Some(("Tyriar".into(), "sort-lines".into()));
        assert_eq!(extension_id("Tyriar.sort-lines"), expected);
        assert_eq!(
            extension_id("https://open-vsx.org/extension/Tyriar/sort-lines"),
            expected
        );
        assert!(extension_id("https://example.com/extension/Tyriar/sort-lines").is_none());
        assert!(extension_id("sort lines").is_none());
    }

    fn package_file(entries: &[(&str, &str)]) -> File {
        let mut archive = ZipWriter::new(tempfile::tempfile().unwrap());
        for (path, data) in entries {
            archive
                .start_file(*path, SimpleFileOptions::default())
                .unwrap();
            archive.write_all(data.as_bytes()).unwrap();
        }
        let mut file = archive.finish().unwrap();
        file.rewind().unwrap();
        file
    }

    #[test]
    fn extracts_and_discover_packages_in_the_installed_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("example.package-1.2.0/extension");
        fs::create_dir_all(&root).unwrap();
        let manifest = r#"{"publisher":"example","name":"package","version":"1.2.0","contributes":{"themes":[{}]}}"#;
        unpack(
            package_file(&[
                ("extension/package.json", manifest),
                ("extension/themes/colors.json", "{}"),
                ("[Content_Types].xml", "<ignored/>"),
            ]),
            &root,
        )
        .unwrap();
        let packages = installed(dir.path());
        let package = &packages["example.package"];
        assert_eq!(package.root, root);
        assert_eq!(package.entry.version, "1.2.0");
        assert!(package.color_theme);
        assert!(root.join("themes/colors.json").is_file());
        assert!(!root.join("[Content_Types].xml").exists());
    }

    #[test]
    fn rejects_paths_outside_the_staged_extension() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("extension");
        fs::create_dir(&root).unwrap();
        assert!(unpack(package_file(&[("extension/../package.json", "{}")]), &root).is_err());
        assert!(!root.join("../package.json").exists());
    }
}
