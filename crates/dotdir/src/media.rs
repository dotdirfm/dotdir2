use crate::{
    archive::{Location, PREVIEW_LIMIT, VirtualFile},
    commands::MediaState,
    filesystems::Filesystems,
    media_native::NativeMedia,
    model::{SortOrder, read_location},
};
use dotdir_core::{copy::CancelToken, watch::FsWatcher};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) const VIEWER_TITLE: &str = "Image & Video Viewer";
pub(crate) const VIEWER_PRIORITY: i32 = 10;
const IMAGE_PATTERNS: &[&str] = &[
    "*.png", "*.jpg", "*.jpeg", "*.gif", "*.bmp", "*.webp", "*.ico", "*.svg", "*.avif", "*.tiff",
    "*.tif",
];
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "m4v", "webm", "ogv", "ogg", "mov"];
#[cfg(any(target_os = "windows", target_os = "linux"))]
const EXTRA_VIDEO_EXTENSIONS: &[&str] = &["mkv", "avi", "mpeg", "mpg", "wmv", "flv"];
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
const EXTRA_VIDEO_EXTENSIONS: &[&str] = &[];

fn video_extensions() -> impl Iterator<Item = &'static str> {
    VIDEO_EXTENSIONS
        .iter()
        .chain(EXTRA_VIDEO_EXTENSIONS)
        .copied()
}

#[derive(Clone)]
pub struct MediaFilter {
    matcher: GlobSet,
}
impl MediaFilter {
    pub fn new(patterns: impl IntoIterator<Item = impl AsRef<str>>) -> anyhow::Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            builder.add(
                GlobBuilder::new(pattern.as_ref())
                    .case_insensitive(true)
                    .build()?,
            );
        }
        Ok(Self {
            matcher: builder.build()?,
        })
    }
    pub(crate) fn bundled() -> &'static Self {
        static FILTER: OnceLock<MediaFilter> = OnceLock::new();
        FILTER.get_or_init(|| {
            let patterns = IMAGE_PATTERNS
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .chain(video_extensions().map(|extension| format!("*.{extension}")));
            Self::new(patterns).expect("valid bundled media patterns")
        })
    }
    pub fn matches(&self, path: &Path) -> bool {
        path.file_name()
            .is_some_and(|name| self.matcher.is_match(Path::new(name)))
    }
    fn gallery(
        &self,
        parent: &Location,
        filesystems: &Filesystems,
        hidden: bool,
        sort: SortOrder,
    ) -> anyhow::Result<Vec<Location>> {
        Ok(read_location(parent, filesystems, hidden, sort)?
            .into_iter()
            .filter(|entry| !entry.directory && (!entry.symlink || entry.path.local().is_some()))
            .map(|entry| entry.path)
            .filter(|path| self.matches(path.name_path()))
            .collect())
    }
}

pub fn image_format(path: &Location) -> Option<ImageFormat> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::Webp),
        "bmp" => Some(ImageFormat::Bmp),
        "ico" => Some(ImageFormat::Ico),
        "svg" => Some(ImageFormat::Svg),
        "tif" | "tiff" => Some(ImageFormat::Tiff),
        _ => None,
    }
}

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            video_extensions()
                .any(|video_extension| extension.eq_ignore_ascii_case(video_extension))
        })
}

#[derive(Default)]
struct Gallery {
    paths: Vec<Location>,
    index: Option<usize>,
}
impl Gallery {
    fn update(&mut self, paths: Vec<Location>, current: &Location) {
        self.index = paths.iter().position(|p| p == current);
        self.paths = paths;
    }
    fn neighbor(&self, delta: isize) -> Option<Location> {
        let next = match self.index {
            Some(i) => i.checked_add_signed(delta)?,
            None if delta > 0 => 0,
            None => self.paths.len().checked_sub(1)?,
        };
        self.paths.get(next).cloned()
    }
    fn counter(&self) -> String {
        match self.index {
            Some(i) => format!("{} / {}", i + 1, self.paths.len()),
            None => format!("— / {}", self.paths.len()),
        }
    }
}

pub enum MediaEvent {
    Changed,
    AvailabilityChanged,
    Close,
}
enum Backend {
    Loading,
    Image(ImageSource),
    Native(NativeMedia),
    Error(String),
}

enum MediaData {
    Image(Arc<Image>),
    Avif(Vec<u8>),
    Video(VirtualFile, String),
}

pub struct MediaViewer {
    pub path: Location,
    filesystems: Filesystems,
    filter: MediaFilter,
    gallery: Gallery,
    hidden: bool,
    sort: SortOrder,
    focus: FocusHandle,
    backend: Backend,
    visible: bool,
    generation: u64,
    image_epoch: usize,
    status: String,
    watched_path: Arc<Mutex<Option<PathBuf>>>,
    _watcher: Option<FsWatcher>,
    watch_task: Option<Task<()>>,
    gallery_task: Option<Task<()>>,
    file_task: Option<Task<()>>,
    file_cancel: CancelToken,
    video_task: Option<Task<()>>,
}
impl EventEmitter<MediaEvent> for MediaViewer {}
impl Focusable for MediaViewer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.backend {
            Backend::Native(n) => n.focus_handle(cx),
            _ => self.focus.clone(),
        }
    }
}
impl MediaViewer {
    pub fn new(
        path: Location,
        filesystems: Filesystems,
        filter: MediaFilter,
        hidden: bool,
        sort: SortOrder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let watched_path = Arc::new(Mutex::new(path.host_path().map(Path::to_path_buf)));
        let changed = Arc::new(AtomicBool::new(false));
        let (tx, rx) = async_channel::bounded(1);
        let (watch_path, current_changed) = (watched_path.clone(), changed.clone());
        let watcher = FsWatcher::new(Arc::new(move |_, _, name| {
            if name.is_some_and(|name| {
                watch_path
                    .lock()
                    .unwrap()
                    .as_deref()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == name)
            }) {
                current_changed.store(true, Ordering::Relaxed);
            }
            let _ = tx.try_send(());
        }))
        .ok();
        if let (Some(watcher), Some(parent)) = (&watcher, path.host_path().and_then(Path::parent)) {
            watcher.add("media", &parent.to_string_lossy());
        }
        let mut this = Self {
            path,
            filesystems,
            filter,
            hidden,
            sort,
            focus: cx.focus_handle(),
            backend: Backend::Loading,
            gallery: Gallery::default(),
            visible: true,
            generation: 0,
            image_epoch: 0,
            status: String::new(),
            watched_path,
            _watcher: watcher,
            watch_task: None,
            gallery_task: None,
            file_task: None,
            file_cancel: CancelToken::new(),
            video_task: None,
        };
        this.watch_task = Some(cx.spawn_in(window, async move |this, cx| {
            while rx.recv().await.is_ok() {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                while rx.try_recv().is_ok() {}
                let reload = changed.swap(false, Ordering::Relaxed);
                if this
                    .update_in(cx, |this, window, cx| {
                        this.load_gallery(window, cx);
                        if reload {
                            let focused = this.focus_handle(cx).contains_focused(window, cx);
                            this.load_current(window, cx);
                            if focused {
                                cx.emit(MediaEvent::Changed);
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        this.load_current(window, cx);
        this.load_gallery(window, cx);
        this
    }
    fn load_gallery(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let filter = self.filter.clone();
        let Some(parent) = self.path.parent() else {
            return;
        };
        let filesystems = self.filesystems.clone();
        let (hidden, sort) = (self.hidden, self.sort);
        let read = cx
            .background_executor()
            .spawn(async move { filter.gallery(&parent, &filesystems, hidden, sort) });
        self.gallery_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = read.await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(paths) => this.gallery.update(paths, &this.path),
                    Err(e) => this.status = format!("Cannot read gallery: {e}"),
                }
                cx.emit(MediaEvent::AvailabilityChanged);
                cx.notify();
            });
        }));
    }
    fn load_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.file_cancel.cancel();
        self.file_task = None;
        self.video_task = None;
        self.image_epoch = self.image_epoch.wrapping_add(1);
        *self.watched_path.lock().unwrap() = self.path.host_path().map(Path::to_path_buf);
        if let Backend::Image(source) = &self.backend {
            source.remove_asset(cx);
        }
        let video = is_video(self.path.name_path());
        let avif = self
            .path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("avif"));
        self.status = if video {
            "Loading video…"
        } else {
            "Loading image…"
        }
        .into();
        self.backend = Backend::Loading;

        if let Some(local) = self.path.local()
            && (!video || !cfg!(any(target_os = "windows", target_os = "linux")))
        {
            if video || avif {
                let native = NativeMedia::new(local, video, window, cx);
                self.set_native(native, video, window, cx);
            } else {
                self.set_image(ImageSource::from(local.to_path_buf()));
            }
            cx.notify();
            return;
        }

        let path = self.path.clone();
        let filesystems = self.filesystems.clone();
        self.file_cancel = CancelToken::new();
        let cancel = self.file_cancel.clone();
        let read = cx.background_executor().spawn(async move {
            if video {
                let extension = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or_default();
                let file = path.open_seekable(&filesystems, &cancel)?;
                Ok(MediaData::Video(file, extension.to_ascii_lowercase()))
            } else {
                let bytes = path.read_limited(&filesystems, PREVIEW_LIMIT, &cancel)?;
                if avif {
                    Ok(MediaData::Avif(bytes))
                } else {
                    let format = image_format(&path)
                        .ok_or_else(|| anyhow::anyhow!("Unsupported image format"))?;
                    Ok(MediaData::Image(Arc::new(Image::from_bytes(format, bytes))))
                }
            }
        });
        self.file_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result: anyhow::Result<MediaData> = read.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let was_focused = this.focus.contains_focused(window, cx);
                match result {
                    Ok(MediaData::Image(image)) => this.set_image(ImageSource::from(image)),
                    Ok(MediaData::Avif(bytes)) => {
                        let native = NativeMedia::from_image(bytes, window, cx);
                        this.set_native(native, false, window, cx);
                    }
                    Ok(MediaData::Video(file, extension)) => {
                        let native = NativeMedia::from_video(file, &extension, window, cx);
                        this.set_native(native, true, window, cx);
                    }
                    Err(error) => this.set_error(format!("{error:#}")),
                }
                if was_focused && matches!(this.backend, Backend::Native(_)) {
                    cx.emit(MediaEvent::Changed);
                } else {
                    cx.emit(MediaEvent::AvailabilityChanged);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_image(&mut self, source: ImageSource) {
        self.backend = Backend::Image(source);
        self.status = "Image · ← / → to navigate".into();
    }

    fn set_error(&mut self, message: String) {
        self.backend = Backend::Error(message);
        self.status = "Media unavailable".into();
    }

    fn set_native(
        &mut self,
        native: anyhow::Result<NativeMedia>,
        video: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match native {
            Ok(mut native) => {
                native.set_visible(self.visible);
                self.backend = Backend::Native(native);
                if !video {
                    self.status = "Image · ← / → to navigate".into();
                }
            }
            Err(error) => self.set_error(format!("{error:#}")),
        }
        if video && matches!(self.backend, Backend::Native(_)) {
            self.video_task = Some(cx.spawn_in(window, async move |this, cx| {
                let started = Instant::now();
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(200))
                        .await;
                    let keep = this.update_in(cx, |this, _, cx| {
                        let result = match &this.backend {
                            Backend::Native(n) => n.playback_status(),
                            _ => return false,
                        };
                        match result {
                            Some(Ok(())) => {
                                this.status = "Video · Space to play/pause · ← / → to navigate".into();
                                cx.emit(MediaEvent::AvailabilityChanged);
                                cx.notify();
                                false
                            }
                            Some(Err(e)) => {
                                this.set_error(format!("Cannot play this video: {e}"));
                                cx.emit(MediaEvent::Changed);
                                cx.notify();
                                false
                            }
                            None if started.elapsed() > Duration::from_secs(20) => {
                                this.set_error("The video could not be loaded. Its format or codec may not be supported.".into());
                                cx.emit(MediaEvent::Changed);
                                cx.notify();
                                false
                            }
                            None => true,
                        }
                    }).unwrap_or(false);
                    if !keep {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }
    pub fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if let Backend::Native(native) = &mut self.backend {
            native.set_visible(visible);
        }
    }
    pub fn command_state(&self) -> MediaState {
        MediaState {
            playable_video: is_video(self.path.name_path())
                && matches!(&self.backend, Backend::Native(n) if matches!(n.playback_status(), Some(Ok(())))),
            previous: self.gallery.neighbor(-1).is_some(),
            next: self.gallery.neighbor(1).is_some(),
        }
    }
    pub fn navigate(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.gallery.neighbor(delta) {
            self.path = path;
            self.gallery.index = self.gallery.paths.iter().position(|p| p == &self.path);
            self.load_current(window, cx);
            window.focus(&self.focus_handle(cx), cx);
            cx.emit(MediaEvent::Changed);
        }
    }
    pub fn command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match id {
            "imageViewer.navigatePrev" => self.navigate(-1, window, cx),
            "imageViewer.navigateNext" => self.navigate(1, window, cx),
            "media-play-pause" => {
                if let Backend::Native(n) = &self.backend {
                    n.toggle_play();
                }
            }
            "media-refresh" => {
                self.load_current(window, cx);
                self.load_gallery(window, cx);
                cx.emit(MediaEvent::Changed);
            }
            "escape" => cx.emit(MediaEvent::Close),
            _ => return false,
        }
        true
    }
    fn arrow(&self, delta: isize, cx: &Context<Self>) -> AnyElement {
        let label = if delta < 0 {
            "Previous media"
        } else {
            "Next media"
        };
        div()
            .id(if delta < 0 {
                "media-prev"
            } else {
                "media-next"
            })
            .role(Role::Button)
            .aria_label(label)
            .absolute()
            .top(relative(0.48))
            .when(delta < 0, |s| s.left(px(8.)))
            .when(delta > 0, |s| s.right(px(8.)))
            .size(px(36.))
            .rounded_full()
            .bg(rgba(0x00000090))
            .text_color(rgb(0xffffff))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .opacity(0.)
            .group_hover("media-surface", |s| s.opacity(1.))
            .child(if delta < 0 { "‹" } else { "›" })
            .on_click(cx.listener(move |this, _, window, cx| this.navigate(delta, window, cx)))
            .into_any_element()
    }
}
impl Drop for MediaViewer {
    fn drop(&mut self) {
        self.file_cancel.cancel();
    }
}

impl Render for MediaViewer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.backend {
            Backend::Loading => div().child(self.status.clone()).into_any_element(),
            Backend::Image(source) => img(source.clone())
                .id(("media-image", self.image_epoch))
                .max_w_full()
                .max_h_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback(|| {
                    div()
                        .p(px(24.))
                        .text_color(rgb(0xffffff))
                        .child("This image could not be decoded.")
                        .into_any_element()
                })
                .into_any_element(),
            Backend::Native(native) => native.view().into_any_element(),
            Backend::Error(error) => div()
                .p(px(24.))
                .text_color(rgb(0xffffff))
                .child(error.clone())
                .into_any_element(),
        };
        let local = self.path.local().map(Path::to_path_buf);
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .key_context("MediaViewer")
            .track_focus(&self.focus)
            .child(
                div()
                    .id("media-surface")
                    .group("media-surface")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(if is_video(self.path.name_path()) {
                        0x000000
                    } else {
                        0x1a1a1a
                    }))
                    .child(content)
                    .child(
                        div()
                            .absolute()
                            .top(px(8.))
                            .right(px(8.))
                            .px(px(10.))
                            .py(px(4.))
                            .rounded_full()
                            .bg(rgba(0x00000090))
                            .text_color(rgb(0xffffff))
                            .text_size(px(12.))
                            .child(self.gallery.counter()),
                    )
                    .when(self.gallery.neighbor(-1).is_some(), |s| {
                        s.child(self.arrow(-1, cx))
                    })
                    .when(self.gallery.neighbor(1).is_some(), |s| {
                        s.child(self.arrow(1, cx))
                    }),
            )
            .child(
                div()
                    .h(px(26.))
                    .flex_shrink_0()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .text_size(px(11.))
                    .gap(px(12.))
                    .child(div().flex_1().overflow_hidden().child(self.status.clone()))
                    .when_some(local, |status, path| {
                        status.child(
                            div()
                                .id("media-external")
                                .role(Role::Button)
                                .aria_label("Open externally")
                                .cursor_pointer()
                                .child("Open externally")
                                .on_click(cx.listener(move |_, _, _, _| {
                                    let path = path.clone();
                                    std::thread::spawn(move || {
                                        let _ = open::that(path);
                                    });
                                })),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Gallery, MediaFilter, is_video};
    use crate::{archive::Location, filesystems::Filesystems, model::SortOrder};
    use std::path::PathBuf;
    #[test]
    fn gallery_filters_formats_and_preserves_directory_sort() {
        let temp = tempfile::tempdir().unwrap();
        let hidden = temp.path().join(".hidden.png");
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0002;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .attributes(FILE_ATTRIBUTE_HIDDEN)
                .open(hidden)
                .unwrap();
        }
        #[cfg(not(windows))]
        std::fs::write(hidden, "").unwrap();
        for name in ["b.MP4", "a.svg", "z.txt"] {
            std::fs::write(temp.path().join(name), "").unwrap();
        }
        std::fs::create_dir(temp.path().join("folder.png")).unwrap();
        let filter = MediaFilter::bundled();
        let paths = filter
            .gallery(
                &temp.path().into(),
                &Filesystems::new(Default::default()),
                false,
                SortOrder::Name,
            )
            .unwrap();
        assert_eq!(
            paths
                .iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["a.svg", "b.MP4"]
        );
        assert!(is_video(paths[1].name_path()));
        assert!(!is_video(paths[0].name_path()));
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            let matroska = std::path::Path::new("video.MKV");
            assert!(filter.matches(matroska));
            assert!(is_video(matroska));
        }
    }
    #[test]
    fn gallery_does_not_wrap_and_recovers_after_current_file_removed() {
        let a = Location::from(PathBuf::from("a.png"));
        let b = Location::from(PathBuf::from("b.mp4"));
        let mut g = Gallery::default();
        g.update(vec![a.clone(), b.clone()], &a);
        assert_eq!(g.counter(), "1 / 2");
        assert!(g.neighbor(-1).is_none());
        assert_eq!(g.neighbor(1), Some(b.clone()));
        g.update(vec![a.clone(), b.clone()], &b);
        assert!(g.neighbor(1).is_none());
        g.update(vec![a.clone()], &b);
        assert_eq!(g.neighbor(1), Some(a));
    }
}
