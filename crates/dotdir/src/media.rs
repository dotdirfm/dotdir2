use crate::{
    commands::MediaState,
    media_native::NativeMedia,
    model::{SortOrder, read_directory},
};
use dotdir_core::watch::FsWatcher;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct MediaFilter {
    matcher: GlobSet,
}
impl MediaFilter {
    pub fn new(patterns: &[String]) -> anyhow::Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            builder.add(GlobBuilder::new(pattern).case_insensitive(true).build()?);
        }
        Ok(Self {
            matcher: builder.build()?,
        })
    }
    pub fn matches(&self, path: &Path) -> bool {
        path.file_name()
            .is_some_and(|name| self.matcher.is_match(Path::new(name)))
    }
    fn gallery(
        &self,
        parent: &Path,
        hidden: bool,
        sort: SortOrder,
    ) -> anyhow::Result<Vec<PathBuf>> {
        Ok(read_directory(parent, hidden, sort)?
            .into_iter()
            .filter(|entry| !entry.directory)
            .filter_map(|entry| {
                entry
                    .path
                    .local()
                    .filter(|path| self.matches(path))
                    .map(PathBuf::from)
            })
            .collect())
    }
}
pub fn is_video(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "mp4" | "m4v" | "mov" | "webm" | "ogv" | "ogg"
    )
}

#[derive(Default)]
struct Gallery {
    paths: Vec<PathBuf>,
    index: Option<usize>,
}
impl Gallery {
    fn update(&mut self, paths: Vec<PathBuf>, current: &Path) {
        self.index = paths.iter().position(|p| p == current);
        self.paths = paths;
    }
    fn neighbor(&self, delta: isize) -> Option<PathBuf> {
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
    Image,
    Native(NativeMedia),
    Error(String),
}

pub struct MediaViewer {
    pub path: PathBuf,
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
    watched_path: Arc<Mutex<PathBuf>>,
    _watcher: Option<FsWatcher>,
    watch_task: Option<Task<()>>,
    gallery_task: Option<Task<()>>,
    video_task: Option<Task<()>>,
}
impl EventEmitter<MediaEvent> for MediaViewer {}
impl Focusable for MediaViewer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.backend {
            Backend::Native(n) => n.view().focus_handle(cx),
            _ => self.focus.clone(),
        }
    }
}
impl MediaViewer {
    pub fn new(
        path: PathBuf,
        filter: MediaFilter,
        hidden: bool,
        sort: SortOrder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let watched_path = Arc::new(Mutex::new(path.clone()));
        let changed = Arc::new(AtomicBool::new(false));
        let (tx, rx) = async_channel::bounded(1);
        let (watch_path, current_changed) = (watched_path.clone(), changed.clone());
        let watcher = FsWatcher::new(Arc::new(move |_, _, name| {
            if name.is_some_and(|name| {
                watch_path
                    .lock()
                    .unwrap()
                    .file_name()
                    .is_some_and(|n| n == name)
            }) {
                current_changed.store(true, Ordering::Relaxed);
            }
            let _ = tx.try_send(());
        }))
        .ok();
        if let (Some(watcher), Some(parent)) = (&watcher, path.parent()) {
            watcher.add("media", &parent.to_string_lossy());
        }
        let mut this = Self {
            path,
            filter,
            hidden,
            sort,
            focus: cx.focus_handle(),
            backend: Backend::Image,
            gallery: Gallery::default(),
            visible: true,
            generation: 0,
            image_epoch: 0,
            status: String::new(),
            watched_path,
            _watcher: watcher,
            watch_task: None,
            gallery_task: None,
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
        let parent = self.path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let (hidden, sort) = (self.hidden, self.sort);
        let read = cx
            .background_executor()
            .spawn(async move { filter.gallery(&parent, hidden, sort) });
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
        self.video_task = None;
        self.image_epoch = self.image_epoch.wrapping_add(1);
        *self.watched_path.lock().unwrap() = self.path.clone();
        ImageSource::from(self.path.clone()).remove_asset(cx);
        let video = is_video(&self.path);
        let avif = self
            .path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("avif"));
        self.status = if video {
            "Loading video…"
        } else {
            "Image · ← / → to navigate"
        }
        .into();
        self.backend = if video || avif {
            match NativeMedia::new(&self.path, video, window, cx) {
                Ok(mut native) => {
                    native.set_visible(self.visible);
                    Backend::Native(native)
                }
                Err(e) => {
                    self.status = "Media unavailable".into();
                    Backend::Error(e.to_string())
                }
            }
        } else {
            Backend::Image
        };
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
                                this.status = "Video unavailable · try Open externally".into();
                                this.backend = Backend::Error(format!("Cannot play this video: {e}"));
                                cx.emit(MediaEvent::Changed);
                                cx.notify();
                                false
                            }
                            None if started.elapsed() > Duration::from_secs(20) => {
                                this.status = "Video unavailable · try Open externally".into();
                                this.backend = Backend::Error("The video could not be loaded. Its format or codec may not be supported by macOS.".into());
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
        self.visible = visible;
        if let Backend::Native(native) = &mut self.backend {
            native.set_visible(visible);
        }
    }
    pub fn command_state(&self) -> MediaState {
        MediaState {
            playable_video: is_video(&self.path)
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
impl Render for MediaViewer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.backend {
            Backend::Image => img(self.path.clone())
                .id(("media-image", self.image_epoch))
                .max_w_full()
                .max_h_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback(|| {
                    div()
                        .p(px(24.))
                        .text_color(rgb(0xffffff))
                        .child("This image could not be decoded. Try Open externally.")
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
        let path = self.path.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .key_context("MediaViewer")
            .track_focus(&self.focus)
            .child(
                div()
                    .h(px(34.))
                    .flex_shrink_0()
                    .px(px(12.))
                    .py(px(8.))
                    .text_size(px(12.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(self.path.display().to_string()),
            )
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
                    .bg(rgb(if is_video(&self.path) {
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
                    .child(
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
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Gallery, MediaFilter, is_video};
    use crate::model::SortOrder;
    use std::path::PathBuf;
    #[test]
    fn gallery_filters_formats_and_preserves_directory_sort() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["b.MP4", "a.svg", "z.txt", ".hidden.png"] {
            std::fs::write(temp.path().join(name), "").unwrap();
        }
        std::fs::create_dir(temp.path().join("folder.png")).unwrap();
        let filter = MediaFilter::new(&["*.png".into(), "*.svg".into(), "*.mp4".into()]).unwrap();
        let paths = filter.gallery(temp.path(), false, SortOrder::Name).unwrap();
        assert_eq!(
            paths
                .iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["a.svg", "b.MP4"]
        );
        assert!(is_video(&paths[1]));
        assert!(!is_video(&paths[0]));
    }
    #[test]
    fn gallery_does_not_wrap_and_recovers_after_current_file_removed() {
        let a = PathBuf::from("a.png");
        let b = PathBuf::from("b.mp4");
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
