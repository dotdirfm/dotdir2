use crate::{
    app::{DotDir, Message, Modal},
    archive::VirtualFile,
    media_native::NativeMedia,
    open_vsx::{self, Detail, Entry, Event, InstallSource, Installed},
    open_vsx_media::{Readme, ReadmeVideo, VIDEO_BLOCK},
    ui::text,
};
use gpui_kit::{
    component::{
        input::Input,
        text::{MarkdownNode, TextView},
    },
    prelude::FluentBuilder,
    *,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use url::Url;

pub(crate) struct MarketVideo {
    block: usize,
    request: u64,
    state: VideoState,
    _task: Task<()>,
}

enum VideoState {
    Loading,
    Playing(NativeMedia),
    Error(String),
}

impl DotDir {
    fn extensions_directory(&self) -> PathBuf {
        self.settings_path.with_file_name("extensions")
    }

    pub(crate) fn open_marketplace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.market.installed = open_vsx::installed(&self.extensions_directory());
        self.show_modal(Modal::OpenVsx, window, cx);
        let query = if self.market.installed_only {
            String::new()
        } else {
            self.market.query.clone()
        };
        let installed = self.market.installed_only;
        self.dialog_input.update(cx, |input, cx| {
            if installed {
                input.set_placeholder("Filter installed extensions", window, cx);
            }
            input.set_value(query.clone(), window, cx);
        });
        if !installed && self.market.page.query.as_deref() != Some(query.as_str()) {
            self.search_marketplace(&query, 0, cx);
        } else {
            self.sync_market_selection(cx);
        }
    }

    pub(crate) fn schedule_market_search(&mut self, cx: &mut Context<Self>) {
        self.market_search_task = None;
        if self.market.installed_only {
            self.sync_market_selection(cx);
            return;
        }
        let query = self.dialog_input.read(cx).value().to_string();
        if query.trim() == self.market.query
            && (self.market.search_request.is_some()
                || (self.market.search_error.is_none()
                    && self.market.page.query.as_deref() == Some(query.trim())))
        {
            return;
        }
        self.market.search_request = None;
        self.market_search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = this.update(cx, |this, cx| {
                if matches!(this.modal, Some(Modal::OpenVsx)) && !this.market.installed_only {
                    this.search_marketplace(&query, 0, cx);
                }
            });
        }));
    }

    pub(crate) fn search_marketplace(
        &mut self,
        query: &str,
        offset: usize,
        cx: &mut Context<Self>,
    ) {
        self.market_search_task = None;
        if self.market.installed_only {
            if let Some(first) = self.market_entries(cx).first().cloned() {
                self.select_market_entry(first, cx);
            }
            return;
        }
        let request = self.id();
        let query = query.trim().to_owned();
        self.market.query.clone_from(&query);
        self.market.search_request = Some(request);
        self.market.search_error = None;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Message::Market(Event::Searched(
                request,
                open_vsx::search(&query, offset),
            )));
        });
        cx.notify();
    }

    fn marketplace_tab(&mut self, installed: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.market.installed_only == installed {
            return;
        }
        self.market.installed_only = installed;
        self.market.selected = None;
        self.market.results_scroll.set_offset(point(px(0.), px(0.)));
        if installed {
            self.market.search_request = None;
            self.market.installed = open_vsx::installed(&self.extensions_directory());
        }
        let query = if installed {
            String::new()
        } else {
            self.market.query.clone()
        };
        self.dialog_input.update(cx, |input, cx| {
            input.set_placeholder(
                if installed {
                    "Filter installed extensions"
                } else {
                    "Search Open VSX"
                },
                window,
                cx,
            );
            input.set_value(query.clone(), window, cx);
            input.focus(window, cx);
        });
        if !installed && self.market.page.query.as_deref() != Some(query.as_str()) {
            self.search_marketplace(&query, 0, cx);
        } else {
            self.sync_market_selection(cx);
        }
        cx.notify();
    }

    fn market_entries(&self, cx: &App) -> Vec<Entry> {
        if !self.market.installed_only {
            return self.market.page.extensions.clone();
        }
        let query = self.dialog_input.read(cx).value().to_lowercase();
        self.market
            .installed
            .values()
            .filter(|package| {
                package.entry.id().contains(&query)
                    || package.entry.title().to_lowercase().contains(&query)
            })
            .map(|package| package.entry.clone())
            .collect()
    }

    fn sync_market_selection(&mut self, cx: &mut Context<Self>) {
        let entries = self.market_entries(cx);
        if let Some(current) = &self.market.selected
            && entries.iter().any(|entry| entry.id() == current.id())
        {
            return;
        }
        if let Some(first) = entries.into_iter().next() {
            self.select_market_entry(first, cx);
        } else {
            self.clear_market_media(cx);
            self.market.selected = None;
            self.market.detail = None;
            self.market.detail_request = None;
            self.market.detail_error = None;
        }
    }

    fn select_market_entry(&mut self, entry: Entry, cx: &mut Context<Self>) {
        if self
            .market
            .selected
            .as_ref()
            .is_some_and(|current| current.id() == entry.id())
            && (self.market.detail.is_some() || self.market.detail_request.is_some())
        {
            return;
        }
        self.clear_market_media(cx);
        self.market.selected = Some(entry.clone());
        self.market.detail = None;
        self.market.detail_error = None;
        self.market.details_scroll.set_offset(point(px(0.), px(0.)));
        if self.market.installed_only
            && self
                .market
                .installed
                .get(&entry.id())
                .is_some_and(|package| package.local_vsix)
        {
            self.market.detail_request = None;
            self.market.detail = Some(Detail {
                release: entry,
                readme: None,
                icon: None,
            });
            cx.notify();
            return;
        }
        let request = self.id();
        self.market.detail_request = Some(request);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Message::Market(Event::Selected(
                request,
                open_vsx::details(&entry).map(Box::new),
            )));
        });
        cx.notify();
    }

    pub(crate) fn clear_market_media(&mut self, cx: &mut Context<Self>) {
        self.market.video = None;
        if let Some(readme) = self
            .market
            .detail
            .as_ref()
            .and_then(|detail| detail.readme.as_ref())
        {
            readme.clear_images(cx);
        }
    }

    fn play_market_video(
        &mut self,
        block: usize,
        video: ReadmeVideo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self.id();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = video.load().await;
            let started = this
                .update_in(cx, |this, window, cx| {
                    let Some(video) = this
                        .market
                        .video
                        .as_mut()
                        .filter(|video| video.request == request)
                    else {
                        return false;
                    };
                    let result = result.and_then(|(bytes, extension)| {
                        NativeMedia::from_video(VirtualFile::memory(bytes), extension, window, cx)
                    });
                    video.state = match result {
                        Ok(native) => VideoState::Playing(native),
                        Err(error) => VideoState::Error(format!("{error:#}")),
                    };
                    cx.notify();
                    matches!(video.state, VideoState::Playing(_))
                })
                .unwrap_or(false);
            if !started {
                return;
            }
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let pending = this
                    .update_in(cx, |this, _, cx| {
                        let Some(video) = this
                            .market
                            .video
                            .as_mut()
                            .filter(|video| video.request == request)
                        else {
                            return false;
                        };
                        let VideoState::Playing(native) = &video.state else {
                            return false;
                        };
                        let error = match native.playback_status() {
                            Some(Ok(())) => return false,
                            Some(Err(error)) => error,
                            None if Instant::now() < deadline => return true,
                            None => "The video codec could not be loaded.".into(),
                        };
                        video.state = VideoState::Error(error);
                        cx.notify();
                        false
                    })
                    .unwrap_or(false);
                if !pending {
                    break;
                }
            }
        });
        self.market.video = Some(MarketVideo {
            block,
            request,
            state: VideoState::Loading,
            _task: task,
        });
        cx.notify();
    }

    pub(crate) fn move_market_selection(&mut self, down: bool, cx: &mut Context<Self>) {
        let entries = self.market_entries(cx);
        let current = self
            .market
            .selected
            .as_ref()
            .and_then(|current| entries.iter().position(|entry| entry.id() == current.id()));
        let index = current.map_or(0, |index| {
            if down {
                (index + 1).min(entries.len().saturating_sub(1))
            } else {
                index.saturating_sub(1)
            }
        });
        if let Some(entry) = entries.into_iter().nth(index) {
            self.select_market_entry(entry, cx);
            self.market.results_scroll.scroll_to_item(index);
        }
    }

    fn install_market_entry(&mut self, cx: &mut Context<Self>) {
        if self.market.installing.is_some() {
            return;
        }
        let Some(entry) = self
            .market
            .detail
            .as_ref()
            .map(|detail| detail.release.clone())
            .filter(Entry::installable)
        else {
            return;
        };
        self.market.install_error = None;
        self.start_extension_install(
            InstallSource::OpenVsx(entry.id()),
            move |directory| open_vsx::install(&entry, directory),
            cx,
        );
    }

    pub(crate) fn choose_vsix(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.market.installing.is_some() {
            return;
        }
        self.market.installing = Some(InstallSource::Vsix);
        self.market.vsix_result = None;
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select a .vsix file".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let selection = selection.await;
            let _ = this.update_in(cx, |this, _, cx| match selection {
                Ok(Ok(Some(mut paths))) if !paths.is_empty() => {
                    let path = paths.remove(0);
                    this.start_extension_install(
                        InstallSource::Vsix,
                        move |directory| open_vsx::install_from_vsix(&path, directory),
                        cx,
                    );
                }
                Ok(Err(error)) => {
                    this.handle_marketplace(Event::Installed(InstallSource::Vsix, Err(error)), cx);
                }
                _ => {
                    this.market.installing = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn start_extension_install(
        &mut self,
        source: InstallSource,
        install: impl FnOnce(&Path) -> anyhow::Result<Installed> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.market.installing = Some(source.clone());
        let tx = self.tx.clone();
        let directory = self.extensions_directory();
        std::thread::spawn(move || {
            let _ = tx.send(Message::Market(Event::Installed(
                source,
                install(&directory).map(Box::new),
            )));
        });
        cx.notify();
    }

    pub(crate) fn handle_marketplace(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Searched(id, result) if self.market.search_request == Some(id) => {
                self.market.search_request = None;
                match result {
                    Ok(page) => {
                        self.market.page = page;
                        self.market.results_scroll.set_offset(point(px(0.), px(0.)));
                        if !self.market.installed_only && matches!(self.modal, Some(Modal::OpenVsx))
                        {
                            self.sync_market_selection(cx);
                        }
                    }
                    Err(error) => self.market.search_error = Some(format!("{error:#}")),
                }
            }
            Event::Selected(id, result) if self.market.detail_request == Some(id) => {
                self.market.detail_request = None;
                match result {
                    Ok(detail) => self.market.detail = Some(*detail),
                    Err(error) => self.market.detail_error = Some(format!("{error:#}")),
                }
            }
            Event::Installed(source, result) => {
                self.market.installing = None;
                match result {
                    Ok(package) => {
                        self.status = format!(
                            "Installed {} {}",
                            package.entry.title(),
                            package.entry.version
                        );
                        if source == InstallSource::Vsix {
                            self.market.vsix_result = Some(Ok(self.status.clone()));
                        }
                        let directory = self.extensions_directory();
                        self.market.installed = open_vsx::installed(&directory);
                        self.load_vscode_extensions(&directory);
                        let reload = self.settings.vscode.enabled(&package.entry.id());
                        if reload {
                            self.reset_vscode_host();
                        }
                        self.refresh_vscode_language_servers(reload, cx);
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        match source {
                            InstallSource::OpenVsx(id) => {
                                self.market.install_error = Some((id, message.clone()));
                                self.error(format!("Open VSX install: {message}"), cx);
                            }
                            InstallSource::Vsix => {
                                self.market.vsix_result = Some(Err(message.clone()));
                                self.error(format!("VSIX install: {message}"), cx);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl DotDir {
    fn market_readme(
        &self,
        id: &str,
        release: &Entry,
        readme: Arc<Readme>,
        cx: &Context<Self>,
    ) -> TextView {
        let registry = Url::parse(&release.page_url()).ok();
        let view = cx.weak_entity();
        let palette = self.palette;
        TextView::markdown(
            SharedString::from(format!("open-vsx-readme-{id}-{}", release.version)),
            readme.body.clone(),
        )
        .selectable(true)
        .code_block_actions(|_, _, _| div().size_0())
        .image_source({
            let readme = readme.clone();
            move |source| readme.image(source.as_ref())
        })
        .markdown_block_parser({
            let readme = readme.clone();
            move |node, _| {
                readme
                    .video(node)
                    .map(|video| MarkdownNode::new(VIDEO_BLOCK, video).accessibility_label("Video"))
            }
        })
        .markdown_block_renderer(VIDEO_BLOCK, move |node, _, cx| {
            let Some(video) = node.data::<ReadmeVideo>() else {
                return div();
            };
            let url = video.url().to_owned();
            let key = node.source_range().map_or(0, |range| range.start);
            let state = view
                .read_with(cx, |this, _| {
                    let playing = this
                        .market
                        .video
                        .as_ref()
                        .filter(|entry| entry.block == key)?;
                    Some(match &playing.state {
                        VideoState::Loading => (None, Some("Loading video…".into()), true),
                        VideoState::Playing(native) => (Some(native.view()), None, true),
                        VideoState::Error(error) => (None, Some(error.clone()), false),
                    })
                })
                .ok()
                .flatten();
            let (native, status, started) = state.unwrap_or_default();
            let control = if started {
                "Close video"
            } else if cfg!(target_os = "macos") {
                "Play video"
            } else {
                "Open video ↗"
            };
            let button = |id, label: &'static str| {
                div()
                    .id((id, key))
                    .role(Role::Button)
                    .aria_label(label)
                    .tab_index(0)
                    .cursor_pointer()
                    .text_color(palette.accent)
                    .hover(move |s| s.bg(palette.selected))
                    .rounded(px(3.))
                    .px(px(8.))
                    .py(px(4.))
                    .child(label)
            };
            let poster = native.is_none().then(|| video.poster.as_deref()).flatten();
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .min_w_0()
                .w_full()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(button("readme-video-control", control).on_click({
                            let view = view.clone();
                            let video = video.clone();
                            move |_, window, cx| {
                                if !cfg!(target_os = "macos") {
                                    cx.open_url(video.url());
                                    return;
                                }
                                let _ = view.update(cx, |this, cx| {
                                    if started {
                                        this.market.video = None;
                                        cx.notify();
                                    } else {
                                        this.play_market_video(key, video.clone(), window, cx);
                                    }
                                });
                            }
                        }))
                        .when(cfg!(target_os = "macos"), |s| {
                            s.child(
                                button("readme-video-external", "Open in browser ↗")
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                        }),
                )
                .when_some(native, |s, native| {
                    s.child(
                        div()
                            .relative()
                            .overflow_hidden()
                            .w_full()
                            .h(px(270.))
                            .child(native),
                    )
                })
                .when_some(poster, |s, poster| {
                    s.child(
                        img(readme.image(poster))
                            .max_w_full()
                            .max_h(px(270.))
                            .object_fit(ObjectFit::Contain),
                    )
                })
                .when_some(status, |s, status| {
                    s.child(text(status).text_color(palette.muted))
                })
        })
        .on_link_click(move |href, _, _, cx| {
            if let Some(url) = Url::parse(href.as_ref())
                .ok()
                .or_else(|| registry.as_ref()?.join(href.as_ref()).ok())
                && url.scheme() == "https"
            {
                cx.open_url(url.as_str());
            }
        })
    }
}

impl DotDir {
    pub(crate) fn render_marketplace(&self, window: &Window, cx: &Context<Self>) -> Div {
        let p = self.palette;
        let entries = self.market_entries(cx);
        let is_empty = entries.is_empty();
        let selected = self.market.selected.as_ref().map(Entry::id);
        let mut list = div()
            .id("open-vsx-results")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.market.results_scroll)
            .role(Role::ListBox)
            .aria_label("Extensions");
        for entry in entries {
            let id = entry.id();
            let is_selected = selected.as_ref() == Some(&id);
            let row_status = if self.market.installing_open_vsx(&id) {
                Some("Installing…")
            } else if self.market.installed.contains_key(&id) {
                Some("Installed")
            } else {
                None
            };
            list = list.child(
                div()
                    .id(SharedString::from(id))
                    .role(Role::ListBoxOption)
                    .aria_label(entry.title().to_owned())
                    .aria_selected(is_selected)
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .min_h(px(63.))
                    .flex_shrink_0()
                    .pl(px(10.))
                    .pr(px(12.))
                    .border_l_2()
                    .border_color(if is_selected { p.accent } else { p.input })
                    .when(is_selected, |s| {
                        s.bg(p.selected).text_color(p.selected_fg.unwrap_or(p.fg))
                    })
                    .hover(move |s| s.bg(p.selected).text_color(p.selected_fg.unwrap_or(p.fg)))
                    .cursor_pointer()
                    .child(
                        self.market_icon(
                            &entry,
                            self.market
                                .detail
                                .as_ref()
                                .filter(|detail| detail.release.id() == entry.id())
                                .and_then(|detail| detail.icon.as_ref()),
                            32.,
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(text(entry.title().to_owned()).font_weight(FontWeight::SEMIBOLD))
                            .child(
                                text(entry.description.clone().unwrap_or_default())
                                    .text_size(px(12.))
                                    .text_color(p.muted),
                            )
                            .child(
                                div()
                                    .flex()
                                    .justify_between()
                                    .gap(px(4.))
                                    .text_size(px(11.))
                                    .child(text(entry.namespace.clone()).min_w_0())
                                    .when_some(row_status, |s, status| {
                                        s.child(text(status).flex_shrink_0().text_color(p.accent))
                                    }),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_market_entry(entry.clone(), cx)
                    })),
            );
        }
        let subtitle = if self.market.installed_only {
            format!("{} installed", self.market.installed.len())
        } else if self.market.search_request.is_some() {
            "Searching Open VSX…".into()
        } else if self.market.query.is_empty() {
            "Popular on Open VSX".into()
        } else {
            format!("{} results", self.market.page.total_size)
        };
        let mut sidebar = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(relative(0.34))
            .max_w(px(330.))
            .min_h_0()
            .bg(p.input)
            .border_r_1()
            .border_color(p.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(10.))
                    .flex_shrink_0()
                    .child(Input::new(&self.dialog_input))
                    .child(div().flex().gap(px(6.)).text_size(px(12.)).children(
                        [("Browse", false), ("Installed", true)].map(|(label, installed)| {
                            self.button(
                                label,
                                label,
                                move |this, window, cx| {
                                    this.marketplace_tab(installed, window, cx);
                                },
                                cx,
                            )
                            .when(self.market.installed_only == installed, |s| {
                                s.bg(p.selected).border_color(p.accent)
                            })
                        }),
                    ))
                    .child(text(subtitle).text_size(px(11.)).text_color(p.muted)),
            );
        if let Some(error) = &self.market.search_error
            && !self.market.installed_only
        {
            sidebar = sidebar.child(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(px(6.))
                    .p(px(10.))
                    .text_size(px(12.))
                    .child(div().text_color(p.error).child(error.clone()))
                    .child(self.button(
                        "open-vsx-retry",
                        "Retry",
                        |this, _, cx| {
                            let query = this.market.query.clone();
                            this.search_marketplace(&query, this.market.page.offset, cx);
                        },
                        cx,
                    )),
            );
        } else if is_empty && self.market.search_request.is_none() {
            sidebar = sidebar.child(
                div()
                    .p(px(10.))
                    .text_color(p.muted)
                    .child("No extensions found"),
            );
        }
        sidebar = sidebar.child(list);
        if !self.market.installed_only
            && self.market.search_request.is_none()
            && self.market.search_error.is_none()
        {
            let previous = self.market.page.previous_offset();
            let next = self.market.page.next_offset();
            if previous.is_some() || next.is_some() {
                sidebar = sidebar.child(
                    div()
                        .flex()
                        .justify_between()
                        .gap(px(6.))
                        .p(px(8.))
                        .border_t_1()
                        .border_color(p.border)
                        .text_size(px(12.))
                        .children(
                            [("Previous", previous), ("Next", next)]
                                .into_iter()
                                .filter_map(|(label, offset)| {
                                    offset.map(|offset| {
                                        self.button(
                                            label,
                                            label,
                                            move |this, _, cx| {
                                                let query = this.market.query.clone();
                                                this.search_marketplace(&query, offset, cx);
                                            },
                                            cx,
                                        )
                                    })
                                }),
                        ),
                );
            }
        }
        let viewport_height: f32 = window.viewport_size().height.into();
        div()
            .flex()
            .h(px((viewport_height * 0.79 - 85.).max(100.)))
            .min_h_0()
            .rounded(px(5.))
            .border_1()
            .border_color(p.border)
            .overflow_hidden()
            .child(sidebar)
            .child(self.render_market_detail(cx))
    }

    fn market_icon(
        &self,
        entry: &Entry,
        icon: Option<&std::sync::Arc<Image>>,
        size: f32,
    ) -> AnyElement {
        let p = self.palette;
        if let Some(icon) = icon {
            return img(icon.clone())
                .size(px(size))
                .flex_shrink_0()
                .rounded(px(5.))
                .object_fit(ObjectFit::Contain)
                .into_any_element();
        }
        div()
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .size(px(size))
            .rounded(px(5.))
            .bg(p.bg)
            .border_1()
            .border_color(p.border)
            .text_color(p.accent)
            .text_size(px(size * 0.49))
            .font_weight(FontWeight::SEMIBOLD)
            .child(
                entry
                    .title()
                    .chars()
                    .next()
                    .map(|initial| initial.to_uppercase().to_string())
                    .unwrap_or_default(),
            )
            .into_any_element()
    }

    fn render_market_detail(&self, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        let mut panel = div().flex().flex_col().flex_1().min_w_0().min_h_0();
        let Some(selected) = &self.market.selected else {
            return panel
                .p(px(18.))
                .text_color(p.muted)
                .child("Select an extension to see its details.")
                .into_any_element();
        };
        let id = selected.id();
        let detail = self.market.detail.as_ref();
        let release = detail.map_or(selected, |detail| &detail.release);
        let installed = self.market.installed.get(&id);
        let mut metadata = release.namespace.clone();
        if !release.version.is_empty() {
            metadata.push_str(&format!("  ·  v{}", release.version));
        }
        if release.pre_release {
            metadata.push_str("  ·  Pre-release");
        }
        if release.download_count > 0 {
            metadata.push_str(&format!("  ·  {} downloads", release.download_count));
        }
        if let Some(rating) = release.average_rating {
            metadata.push_str(&format!("  ·  ★ {rating:.1}"));
        }
        let page = release.page_url();
        let external = page.clone();
        let local_vsix =
            self.market.installed_only && installed.is_some_and(|package| package.local_vsix);
        let mut actions =
            self.market_install_controls(release, installed, cx)
                .when(!local_vsix, |actions| {
                    actions.child(self.button(
                        "open-vsx-page",
                        "Open in Open VSX ↗",
                        move |_, _, cx| cx.open_url(&external),
                        cx,
                    ))
                });
        if let Some(package) = installed {
            if package.color_theme {
                let root = package.root.clone();
                actions = actions.child(self.button(
                    "open-vsx-color-theme",
                    "Use Color Theme…",
                    move |this, window, cx| {
                        if let Err(error) = this.choose_color_theme(root.clone(), window, cx) {
                            this.error(format!("{error:#}"), cx);
                        }
                    },
                    cx,
                ));
            }
            if package.icon_theme {
                let root = package.root.clone();
                actions = actions.child(self.button(
                    "open-vsx-icon-theme",
                    "Use File Icons",
                    move |this, _, cx| {
                        this.load_icon_theme(root.clone(), cx);
                    },
                    cx,
                ));
            }
        }
        panel = panel.child(
            div()
                .flex()
                .items_start()
                .gap(px(15.))
                .p(px(16.))
                .flex_shrink_0()
                .border_b_1()
                .border_color(p.border)
                .child(self.market_icon(
                    release,
                    detail.and_then(|detail| detail.icon.as_ref()),
                    62.,
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(px(6.))
                        .child(
                            div()
                                .text_size(px(24.))
                                .line_height(px(28.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(release.title().to_owned()),
                        )
                        .child(div().text_size(px(12.)).text_color(p.muted).child(metadata))
                        .when_some(release.description.clone(), |s, description| {
                            s.child(div().child(description))
                        })
                        .child(actions)
                        .when_some(
                            self.market
                                .install_error
                                .as_ref()
                                .filter(|(failed, _)| failed == &id),
                            |s, (_, error)| {
                                s.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(p.error)
                                        .child(error.clone()),
                                )
                            },
                        ),
                ),
        );
        panel
            .child(
                div()
                    .id("open-vsx-details")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(px(12.))
                    .overflow_y_scroll()
                    .track_scroll(&self.market.details_scroll)
                    .p(px(16.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.))
                            .child(text("Details").font_weight(FontWeight::SEMIBOLD))
                            .child(
                                text(id.clone())
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .text_color(p.muted),
                            ),
                    )
                    .when_some(
                        release
                            .target_platform
                            .clone()
                            .filter(|target| target != "universal"),
                        |s, target| {
                            s.child(
                                text(format!("Platform  {target}"))
                                    .text_size(px(12.))
                                    .text_color(p.muted),
                            )
                        },
                    )
                    .when_some(
                        installed.filter(|package| package.entry.version != release.version),
                        |s, package| {
                            s.child(
                                text(format!("Installed version  {}", package.entry.version))
                                    .text_size(px(12.))
                                    .text_color(p.muted),
                            )
                        },
                    )
                    .when(self.market.detail_request.is_some(), |s| {
                        s.child(text("Loading details…").text_color(p.muted))
                    })
                    .when_some(self.market.detail_error.as_ref(), |s, error| {
                        s.child(
                            div()
                                .text_size(px(12.))
                                .text_color(p.muted)
                                .child(error.clone()),
                        )
                        .child(div().flex().child(self.button(
                            "open-vsx-retry-details",
                            "Retry details",
                            |this, _, cx| {
                                if let Some(entry) = this.market.selected.clone() {
                                    this.select_market_entry(entry, cx);
                                }
                            },
                            cx,
                        )))
                    })
                    .when_some(
                        detail.and_then(|detail| detail.readme.clone()),
                        |s, readme| s.child(self.market_readme(&id, release, readme, cx)),
                    ),
            )
            .into_any_element()
    }

    fn market_install_controls(
        &self,
        release: &Entry,
        installed: Option<&Installed>,
        cx: &Context<Self>,
    ) -> Div {
        let p = self.palette;
        let id = release.id();
        let mut actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .text_size(px(12.));
        if self.market.installing_open_vsx(&id) {
            return actions.child(text("Installing…").text_color(p.accent));
        }
        let update = installed.is_some_and(|package| {
            crate::vscode::version_key(&release.version)
                > crate::vscode::version_key(&package.entry.version)
        });
        if self.market.detail.is_some() && release.installable() && (installed.is_none() || update)
        {
            let label = if update { "Update" } else { "Install" };
            actions = if self.market.installing.is_none() {
                actions.child(self.primary_button(
                    "open-vsx-install",
                    label,
                    |this, _, cx| this.install_market_entry(cx),
                    cx,
                ))
            } else {
                actions.child(text("Another installation is running").text_color(p.muted))
            };
        } else if self.market.detail.is_some() && !release.installable() && installed.is_none() {
            actions = actions.child(text("Unavailable for this platform").text_color(p.muted));
        }
        if installed.is_some() {
            actions = actions.child(text("Installed").text_color(p.accent));
            if self
                .vscode_extensions
                .iter()
                .any(|extension| extension.id.eq_ignore_ascii_case(&id))
            {
                let label = if self.settings.vscode.enabled(&id) {
                    "Disable"
                } else {
                    "Enable"
                };
                actions = actions.child(self.button(
                    "open-vsx-enable",
                    label,
                    move |this, _, cx| {
                        this.toggle_vscode_extension(&id, cx);
                    },
                    cx,
                ));
            }
        }
        actions
    }
}
