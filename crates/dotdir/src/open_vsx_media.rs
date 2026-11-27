use crate::open_vsx::read_bytes;
use anyhow::{Context as _, Result, bail, ensure};
use gpui_kit::{App, Asset, ImageCacheError, ImageSource, RenderImage, SharedString, Window};
use html5ever::{local_name, namespace_url, ns, parse_fragment, tendril::TendrilSink};
use image::{AnimationDecoder, Delay, Frame, ImageDecoder, ImageFormat, ImageReader, Limits};
use markdown::mdast::Node;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use smallvec::{SmallVec, smallvec};
use std::{
    collections::HashSet,
    future::Future,
    io::Cursor,
    net::IpAddr,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use ureq::unversioned::{
    resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver},
    transport::{DefaultConnector, NextTimeout},
};
use url::Url;

const IMAGE_LIMIT: u64 = 16 * 1024 * 1024;
const VIDEO_LIMIT: u64 = 64 * 1024 * 1024;
const DECODE_LIMIT: usize = 48 * 1024 * 1024;
const IMAGE_COUNT: usize = 24;
pub(crate) const VIDEO_BLOCK: &str = "open-vsx-video";

pub(crate) struct Readme {
    pub body: SharedString,
    base: Url,
    repository: Option<Url>,
    images: Mutex<HashSet<MediaRequest>>,
}

impl Readme {
    pub fn new(body: String, base: Url, repository: Option<&str>) -> Self {
        Self {
            body: body.into(),
            base,
            repository: repository.and_then(repository_base),
            images: Mutex::new(HashSet::new()),
        }
    }

    fn request(&self, source: &str) -> Option<MediaRequest> {
        let source = source.trim();
        if source.is_empty() || source.starts_with('#') {
            return None;
        }
        let primary = normalize_media_url(self.base.join(source).ok()?)?;
        let mut urls = vec![primary];
        if Url::parse(source).is_err()
            && !source.starts_with("//")
            && let Some(repository) = &self.repository
            && let Some(fallback) = repository
                .join(source.trim_start_matches('/'))
                .ok()
                .and_then(normalize_media_url)
            && urls[0] != fallback
        {
            urls.push(fallback);
        }
        Some(MediaRequest(urls))
    }

    pub fn image(&self, source: &str) -> ImageSource {
        let Some(request) = self.request(source) else {
            return ImageSource::from(|_: &mut Window, _: &mut App| None);
        };
        let mut images = self.images.lock().unwrap();
        if images.len() >= IMAGE_COUNT && !images.contains(&request) {
            return ImageSource::from(|_: &mut Window, _: &mut App| None);
        }
        images.insert(request.clone());
        ImageSource::from(move |window: &mut Window, cx: &mut App| {
            window.use_asset::<ReadmeImage>(&request, cx)
        })
    }

    pub fn clear_images(&self, cx: &mut App) {
        // Take the keys first. Removing an asset can invalidate a view.
        let images = std::mem::take(&mut *self.images.lock().unwrap());
        for request in images {
            cx.remove_asset::<ReadmeImage>(&request);
        }
    }

    pub fn video(&self, node: &Node) -> Option<ReadmeVideo> {
        let (sources, poster) = match node {
            Node::Html(html) if contains_video_tag(&html.value) => html_video(&html.value)?,
            Node::Paragraph(paragraph) => match paragraph.children.as_slice() {
                [Node::Link(link)] if looks_like_video(&self.base.join(&link.url).ok()?) => {
                    (vec![link.url.clone()], None)
                }
                [Node::Text(text)] if looks_like_video(&Url::parse(text.value.trim()).ok()?) => {
                    (vec![text.value.trim().to_owned()], None)
                }
                children
                    if children.iter().any(
                        |node| matches!(node, Node::Html(html) if contains_video_tag(&html.value)),
                    ) =>
                {
                    let html = children
                        .iter()
                        .map(|node| match node {
                            Node::Html(html) => Some(html.value.as_str()),
                            Node::Text(text) if text.value.trim().is_empty() => {
                                Some(text.value.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>()?
                        .concat();
                    html_video(&html)?
                }
                _ => return None,
            },
            _ => return None,
        };
        let mut urls = Vec::new();
        for request in sources
            .iter()
            .take(4)
            .filter_map(|source| self.request(source))
        {
            for url in request.0 {
                if !urls.contains(&url) {
                    urls.push(url);
                }
            }
        }
        if urls.is_empty() {
            return None;
        }
        Some(ReadmeVideo {
            request: MediaRequest(urls),
            poster,
        })
    }
}

#[derive(Clone)]
pub(crate) struct ReadmeVideo {
    request: MediaRequest,
    pub poster: Option<String>,
}

impl ReadmeVideo {
    pub fn url(&self) -> &str {
        // Relative media usually lives in the publisher's repository.
        self.request.0.last().expect("video source").as_str()
    }

    pub fn load(&self) -> impl Future<Output = Result<(Vec<u8>, &'static str)>> + Send + 'static {
        let request = self.request.clone();
        media_job(move || {
            request.try_sources(|url| {
                let (bytes, content_type) = download(url, VIDEO_LIMIT)?;
                let extension = video_extension(&bytes, &content_type)
                    .context("The link did not return a supported video file")?;
                Ok((bytes, extension))
            })
        })
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct MediaRequest(Vec<Url>);

impl MediaRequest {
    fn try_sources<T>(&self, load: impl Fn(&Url) -> Result<T>) -> Result<T> {
        let mut error = None;
        for url in &self.0 {
            match load(url) {
                Ok(value) => return Ok(value),
                Err(failed) => error = Some(failed),
            }
        }
        Err(error.unwrap_or_else(|| anyhow::anyhow!("Missing media source")))
    }
}

struct ReadmeImage;

impl Asset for ReadmeImage {
    type Source = MediaRequest;
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        source: Self::Source,
        _: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        async move {
            media_job(move || {
                source.try_sources(|url| {
                    let (bytes, content_type) = download(url, IMAGE_LIMIT)?;
                    decode_image(
                        &bytes,
                        content_type == "image/svg+xml" || url.path().ends_with(".svg"),
                    )
                })
            })
            .await
            .map_err(Into::into)
        }
    }
}

async fn media_job<T: Send + 'static>(
    run: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    type Job = Box<dyn FnOnce() + Send>;
    static WORK: OnceLock<async_channel::Sender<Job>> = OnceLock::new();
    let queue = WORK.get_or_init(|| {
        let (queue, jobs) = async_channel::bounded::<Job>(IMAGE_COUNT);
        for _ in 0..3 {
            let jobs = jobs.clone();
            std::thread::spawn(move || {
                while let Ok(job) = jobs.recv_blocking() {
                    job();
                }
            });
        }
        queue
    });
    let (send, receive) = async_channel::bounded(1);
    queue
        .send(Box::new(move || {
            if !send.is_closed() {
                let _ = send.try_send(run());
            }
        }))
        .await
        .map_err(|_| anyhow::anyhow!("Media loader is closed"))?;
    receive.recv().await.context("Media loader is closed")?
}

fn repository_base(repository: &str) -> Option<Url> {
    let repository = Url::parse(repository.strip_prefix("git+").unwrap_or(repository)).ok()?;
    if repository.scheme() != "https" {
        return None;
    }
    let path = repository.path().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.split('/').count() < 2 {
        return None;
    }
    match repository.host_str()? {
        "github.com" => Url::parse(&format!("https://raw.githubusercontent.com/{path}/HEAD/")).ok(),
        "gitlab.com" => Url::parse(&format!("https://gitlab.com/{path}/-/raw/HEAD/")).ok(),
        _ => None,
    }
}

fn normalize_media_url(mut url: Url) -> Option<Url> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.host_str().is_none()
    {
        return None;
    }
    if let Some(host) = url.host_str()
        && (host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local"))
    {
        return None;
    }
    if !match url.host()? {
        url::Host::Ipv4(ip) => is_public(ip.into()),
        url::Host::Ipv6(ip) => is_public(ip.into()),
        url::Host::Domain(_) => true,
    } {
        return None;
    }
    url.set_fragment(None);
    if url.host_str() == Some("github.com") {
        let parts = url.path_segments()?.collect::<Vec<_>>();
        if let [owner, repo, "blob", path @ ..] = parts.as_slice()
            && path.len() >= 2
        {
            let url = Url::parse(&format!(
                "https://raw.githubusercontent.com/{owner}/{repo}/{}",
                path.join("/")
            ))
            .ok()?;
            return Some(url);
        }
    }
    Some(url)
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_documentation()
                && a != 0
                && a < 224
                && !matches!(
                    (a, b, c),
                    (100, 64..=127, _) | (192, 0, 0) | (192, 88, 99) | (198, 18..=19, _)
                )
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public(v4.into());
            }
            let [a, b, ..] = ip.segments();
            a & 0xe000 == 0x2000 && a != 0x2002 && !(a == 0x2001 && (b <= 0x01ff || b == 0x0db8))
        }
    }
}

#[derive(Debug)]
struct PublicResolver;

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let resolved = DefaultResolver::default().resolve(uri, config, timeout)?;
        let mut public = self.empty();
        for address in resolved
            .into_iter()
            .filter(|address| is_public(address.ip()))
        {
            public.push(*address);
        }
        if public.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(public)
    }
}

fn download(url: &Url, limit: u64) -> Result<(Vec<u8>, String)> {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    let agent = AGENT.get_or_init(|| {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .proxy(None)
            .timeout_connect(Some(Duration::from_secs(6)))
            .timeout_per_read(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(30)))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build();
        ureq::Agent::with_parts(config, DefaultConnector::default(), PublicResolver)
    });
    let mut url = url.clone();
    for _ in 0..5 {
        let mut response = agent
            .get(url.as_str())
            .header("User-Agent", "DotDir/0.1 (Open VSX media)")
            .call()?;
        let headers = response.headers();
        if response.status().is_redirection() {
            let location = headers
                .get("location")
                .context("Missing media redirect")?
                .to_str()?;
            url = normalize_media_url(url.join(location)?).context("Unsupported media redirect")?;
            continue;
        }
        ensure!(
            response.status().is_success(),
            "Media could not be downloaded"
        );
        if let Some(length) = headers
            .get("content-length")
            .and_then(|header| header.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            ensure!(length <= limit, "The media exceeds the download limit");
        }
        let content_type = headers
            .get("content-type")
            .and_then(|header| header.to_str().ok())
            .and_then(|value| value.split(';').next())
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        return Ok((
            read_bytes(response.body_mut().as_reader(), limit)?,
            content_type,
        ));
    }
    bail!("Too many media redirects")
}

fn decode_image(bytes: &[u8], svg: bool) -> Result<Arc<RenderImage>> {
    if svg {
        return decode_svg(bytes);
    }
    let format = image::guess_format(bytes)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(DECODE_LIMIT as u64);
    let data = Cursor::new(bytes);
    let frames = match format {
        ImageFormat::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(data)?;
            decoder.set_limits(limits)?;
            decode_frames(decoder)?
        }
        ImageFormat::WebP => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(data.clone())?;
            decoder.set_limits(limits.clone())?;
            if decoder.has_animation() {
                decode_frames(decoder)?
            } else {
                still_frame(data, format, limits)?
            }
        }
        ImageFormat::Png | ImageFormat::Jpeg => still_frame(data, format, limits)?,
        _ => bail!("Unsupported README image format"),
    };
    Ok(Arc::new(RenderImage::new(frames)))
}

fn still_frame(
    data: Cursor<&[u8]>,
    format: ImageFormat,
    limits: Limits,
) -> Result<SmallVec<[Frame; 1]>> {
    let mut reader = ImageReader::with_format(data, format);
    reader.limits(limits);
    let mut image = reader.decode()?;
    if image.width() > 1600 || image.height() > 2400 {
        image = image.thumbnail(1600, 2400);
    }
    let mut frame = Frame::new(image.into_rgba8());
    to_bgra(&mut frame);
    Ok(smallvec![frame])
}

fn decode_frames<'a>(
    decoder: impl AnimationDecoder<'a> + ImageDecoder,
) -> Result<SmallVec<[Frame; 1]>> {
    let (width, height) = decoder.dimensions();
    let scale = (760. / width as f64).min(540. / height as f64).min(1.);
    let preview_width = ((width as f64 * scale).round() as u32).max(1);
    let preview_height = ((height as f64 * scale).round() as u32).max(1);
    let max_frames =
        (DECODE_LIMIT / (preview_width as usize * preview_height as usize * 4)).min(300);
    let scan_limit = (700_000_000 / (width as usize * height as usize).max(1)).clamp(1, 1400);
    let mut samples: Vec<(image::RgbaImage, Duration)> = Vec::new();
    let mut stride = 1;
    for (index, frame) in decoder.into_frames().take(scan_limit).enumerate() {
        let frame = frame?;
        let delay = Duration::from(frame.delay());
        // Keep the animation's timeline when long demos exceed the frame budget.
        if samples.len() >= max_frames && index % stride == 0 {
            for (index, sample) in std::mem::take(&mut samples).into_iter().enumerate() {
                if index % 2 == 0 {
                    samples.push(sample);
                } else if let Some((_, delay)) = samples.last_mut() {
                    *delay += sample.1;
                }
            }
            stride *= 2;
        }
        if index % stride == 0 {
            let image = frame.into_buffer();
            let image = if preview_width < width || preview_height < height {
                image::imageops::thumbnail(&image, preview_width, preview_height)
            } else {
                image
            };
            samples.push((image, delay));
        } else if let Some((_, previous)) = samples.last_mut() {
            *previous += delay;
        }
    }
    ensure!(!samples.is_empty(), "The animation has no frames");
    Ok(samples
        .into_iter()
        .map(|(image, delay)| {
            let mut frame = Frame::from_parts(image, 0, 0, Delay::from_saturating_duration(delay));
            to_bgra(&mut frame);
            frame
        })
        .collect())
}

fn to_bgra(frame: &mut Frame) {
    for pixel in frame.buffer_mut().chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

fn decode_svg(bytes: &[u8]) -> Result<Arc<RenderImage>> {
    use resvg::{tiny_skia, usvg};
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    ensure!(
        bytes.len() <= 1024 * 1024 && !bytes.starts_with(b"\x1f\x8b"),
        "SVG exceeds the size limit"
    );
    let options = usvg::Options {
        fontdb: FONTS
            .get_or_init(|| {
                let mut fonts = usvg::fontdb::Database::new();
                fonts.load_system_fonts();
                Arc::new(fonts)
            })
            .clone(),
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options)?;
    let size = tree.size();
    let scale = (1600. / size.width()).min(2400. / size.height()).min(1.);
    let width = (size.width() * scale).ceil() as u32;
    let height = (size.height() * scale).ceil() as u32;
    let mut buffer = tiny_skia::Pixmap::new(width, height).context("Invalid SVG image size")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut buffer.as_mut(),
    );
    let mut buffer = buffer.take();
    for p in buffer.chunks_exact_mut(4) {
        if p[3] != 0 && p[3] != 255 {
            let a = u32::from(p[3]);
            for value in &mut p[..3] {
                *value = ((u32::from(*value) * 255 + a / 2) / a).min(255) as u8;
            }
        }
        p.swap(0, 2);
    }
    let image = image::RgbaImage::from_raw(width, height, buffer).context("Invalid SVG data")?;
    Ok(Arc::new(RenderImage::new(smallvec![Frame::new(image)])))
}

fn looks_like_video(url: &Url) -> bool {
    let extension = url
        .path()
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    matches!(extension.as_deref(), Some("mp4" | "m4v" | "mov" | "webm"))
        || url.host_str() == Some("github.com")
            && url.path().starts_with("/user-attachments/assets/")
}

fn contains_video_tag(html: &str) -> bool {
    html.to_ascii_lowercase().contains("<video")
}

fn video_extension(bytes: &[u8], content_type: &str) -> Option<&'static str> {
    if bytes.starts_with(b"\x1a\x45\xdf\xa3") && content_type != "text/html" {
        return Some("webm");
    }
    if matches!(bytes.get(4..8), Some(b"ftyp" | b"moov" | b"mdat")) {
        return Some(
            if content_type == "video/quicktime" || bytes.get(8..12) == Some(b"qt  ") {
                "mov"
            } else {
                "mp4"
            },
        );
    }
    None
}

fn html_video(html: &str) -> Option<(Vec<String>, Option<String>)> {
    let dom = parse_fragment(
        RcDom::default(),
        Default::default(),
        html5ever::QualName::new(None, ns!(html), local_name!("div")),
        vec![],
    )
    .one(html);
    let mut pending = vec![dom.document.clone()];
    while let Some(node) = pending.pop() {
        let children = node.children.borrow();
        if let NodeData::Element { name, .. } = &node.data
            && name.local == local_name!("video")
        {
            let mut sources: Vec<_> = attribute(&node, "src").into_iter().collect();
            sources.extend(children.iter().filter_map(|child| match &child.data {
                NodeData::Element { name, .. } if name.local == local_name!("source") => {
                    attribute(child, "src")
                }
                _ => None,
            }));
            if !sources.is_empty() {
                return Some((sources, attribute(&node, "poster")));
            }
        }
        pending.extend(children.iter().rev().cloned());
    }
    None
}

fn attribute(node: &Handle, name: &str) -> Option<String> {
    let NodeData::Element { attrs, .. } = &node.data else {
        return None;
    };
    attrs
        .borrow()
        .iter()
        .find_map(|attr| (attr.name.local.as_ref() == name).then(|| attr.value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readme() -> Readme {
        Readme::new(
            String::new(),
            Url::parse("https://open-vsx.org/api/owner/example/1.0/file/readme.md").unwrap(),
            Some("https://github.com/owner/example.git"),
        )
    }

    #[test]
    fn readme_media_resolves_relative_assets_and_github_links() {
        let readme = readme();
        let requests = readme.request("images/screen%20shot.png").unwrap().0;
        assert_eq!(
            requests[0].path(),
            "/api/owner/example/1.0/file/images/screen%20shot.png"
        );
        assert_eq!(
            requests[1].as_str(),
            "https://raw.githubusercontent.com/owner/example/HEAD/images/screen%20shot.png"
        );
        assert_eq!(
            readme
                .request("https://github.com/owner/example/blob/main/demo.gif?raw=true#anchor")
                .unwrap()
                .0[0]
                .as_str(),
            "https://raw.githubusercontent.com/owner/example/main/demo.gif",
        );
        for invalid in [
            "file:///tmp/photo.png",
            "data:image/png;base64,abcd",
            "http://example.com/media",
            "https://127.0.0.1/image.png",
            "https://[::ffff:192.168.0.1]/image.png",
            "https://test.local/image.png",
            "https://user@example.com/image.png",
        ] {
            assert!(readme.request(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn finds_video_elements_and_direct_links_without_parsing_code_blocks() {
        let sample =
            "<video poster='poster.png' controls><source src='demo.mp4?a=1&amp;b=2'></video>";
        assert!(html_video(sample).is_some());
        assert!(readme().request("demo.mp4?a=1&b=2").is_some());
        let document = markdown::to_mdast(
            concat!(
                "<video poster='poster.png' controls><source src='demo.mp4?a=1&amp;b=2'></video>\n\n",
                "[Demo](https://example.com/demo.mov)\n\n",
                "https://github.com/user-attachments/assets/example\n\n",
                "```html\n<video src=\"https://example.com/skip.mp4\"></video>\n```\n",
            ),
            &markdown::ParseOptions::gfm(),
        ).unwrap();
        let readme = readme();
        let videos: Vec<_> = document
            .children()
            .unwrap()
            .iter()
            .filter_map(|node| readme.video(node))
            .collect();
        assert_eq!(videos.len(), 3, "{document:#?}");
        assert_eq!(videos[0].poster.as_deref(), Some("poster.png"));
        assert!(videos[0].url().ends_with("/demo.mp4?a=1&b=2"));
        assert!(videos[1].url().ends_with("/demo.mov"));
        assert!(videos[2].url().contains("/user-attachments/assets/"));
        assert_eq!(
            video_extension(b"\x00\x00\x00\x1cftypisom", "application/octet-stream"),
            Some("mp4")
        );
        assert_eq!(video_extension(b"<html></html>", "text/html"), None);
    }

    #[test]
    fn preserves_animation_frames_and_timing() {
        let mut bytes = Vec::new();
        {
            let mut gif = image::codecs::gif::GifEncoder::new(&mut bytes);
            for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
                gif.encode_frame(Frame::from_parts(
                    image::RgbaImage::from_pixel(20, 20, image::Rgba(color)),
                    0,
                    0,
                    Delay::from_numer_denom_ms(120, 1),
                ))
                .unwrap();
            }
        }
        let image = decode_image(&bytes, false).unwrap();
        assert_eq!(image.frame_count(), 2);
        assert_eq!(image.delay(1).numer_denom_ms(), (120, 1));
    }
}
