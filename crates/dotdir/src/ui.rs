use crate::{
    app::*,
    archive::Location,
    commands::{COMMANDS, function_key_command},
    media::VIEWER_TITLE,
    model::*,
    operations::OperationKind,
    selection_inspector, terminal,
    theme::Palette,
};
use dotdir_core::copy::{ConflictPolicy, ConflictResolution, SymlinkMode};
use dotdir_extension_host::PanelNode;
use gpui_kit::{
    component::{
        Icon, IconName,
        button::Button,
        checkbox::Checkbox,
        input::{Editor, Input, Redo, Undo},
        menu::{DropdownMenu, PopupMenuItem},
    },
    prelude::FluentBuilder,
    *,
};
use serde_json::json;
use std::{ffi::OsStr, time::Duration};

struct TabDrag {
    id: u64,
}

struct TabDragPreview {
    label: String,
    icon: TabIcon,
    palette: Palette,
}

#[derive(Clone)]
enum TabIcon {
    Themed(ImageSource),
    File { directory: bool },
    Glyph(IconName),
}

impl TabIcon {
    fn render(self, palette: Palette) -> AnyElement {
        match self {
            Self::Themed(source) => icon_image(source, 16.),
            Self::File { directory } => {
                let color = if directory {
                    palette.folder
                } else {
                    palette.muted
                };
                file_icon(directory, color)
            }
            Self::Glyph(name) => icon(name, palette.muted).flex_shrink_0().into_any_element(),
        }
    }
}

impl Render for TabDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(28.))
            .max_w(px(200.))
            .px(px(8.))
            .border_1()
            .border_color(p.accent)
            .bg(p.bg)
            .text_color(p.fg)
            .text_size(px(12.))
            .child(self.icon.clone().render(p))
            .child(text(self.label.clone()))
    }
}

pub(crate) fn icon(name: IconName, color: Hsla) -> Icon {
    Icon::new(name).size(px(16.)).text_color(color)
}
fn icon_image(source: ImageSource, size: f32) -> AnyElement {
    img(source)
        .size(px(size))
        .flex_shrink_0()
        .object_fit(ObjectFit::Contain)
        .into_any_element()
}
fn file_icon(directory: bool, color: Hsla) -> AnyElement {
    if directory {
        div()
            .relative()
            .size(px(16.))
            .flex_shrink_0()
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(3.))
                    .w(px(7.))
                    .h(px(4.))
                    .rounded(px(1.))
                    .bg(color),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(5.))
                    .w(px(16.))
                    .h(px(10.))
                    .rounded(px(1.))
                    .bg(color),
            )
            .into_any_element()
    } else {
        icon(IconName::File, color)
            .flex_shrink_0()
            .into_any_element()
    }
}
pub(crate) fn text(value: impl Into<SharedString>) -> Div {
    div()
        .overflow_hidden()
        .text_ellipsis()
        .whitespace_nowrap()
        .child(value.into())
}
impl DotDir {
    fn icon_theme_controls(&self, prefix: &'static str, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(text(format!(
                "File icons: {}",
                self.icon_theme
                    .as_ref()
                    .map(|theme| format!("{} {}", theme.name, theme.version))
                    .unwrap_or_else(|| "Built-in".into())
            )))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(self.button(
                        (prefix, 0usize),
                        "Load icon theme…",
                        |this, window, cx| this.prompt_icon_theme(window, cx),
                        cx,
                    ))
                    .child(self.button(
                        (prefix, 1usize),
                        "Use built-in icons",
                        |this, _, cx| this.clear_icon_theme(cx),
                        cx,
                    )),
            )
    }
    pub(crate) fn entry_icon(&self, entry: &Entry) -> AnyElement {
        self.entry_icon_sized(entry, 16.)
    }
    fn themed_file_icon(&self, name: &str, directory: bool, size: f32) -> Option<ImageSource> {
        let path = self
            .icon_theme
            .as_ref()?
            .icon(name, directory, !self.palette.mode.is_dark())?;
        Some(
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
            {
                crate::icon_theme::sized_icon(path, size)
            } else {
                ImageSource::from(path.to_path_buf())
            },
        )
    }
    fn file_tab_label(&self, name: &OsStr, directory: bool) -> (String, TabIcon) {
        let label = name.to_string_lossy().into_owned();
        let icon = self
            .themed_file_icon(&label, directory, 16.)
            .map(TabIcon::Themed)
            .unwrap_or(TabIcon::File { directory });
        (label, icon)
    }
    pub(crate) fn entry_icon_sized(&self, entry: &Entry, size: f32) -> AnyElement {
        if !entry.parent
            && let Some(source) = self.themed_file_icon(&entry.name, entry.directory, size)
        {
            return icon_image(source, size);
        }
        if size > 16. {
            if entry.directory {
                return div()
                    .relative()
                    .size(px(size))
                    .flex_shrink_0()
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px(size * 0.18))
                            .w(px(size * 0.44))
                            .h(px(size * 0.28))
                            .rounded(px(4.))
                            .bg(self.palette.folder),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px(size * 0.3))
                            .w(px(size))
                            .h(px(size * 0.62))
                            .rounded(px(5.))
                            .bg(self.palette.folder),
                    )
                    .into_any_element();
            }
            return Icon::new(IconName::File)
                .size(px(size))
                .text_color(self.palette.muted)
                .into_any_element();
        }
        file_icon(
            entry.directory,
            if entry.directory {
                self.palette.folder
            } else {
                self.palette.muted
            },
        )
    }
    pub(crate) fn button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        self.styled_button(id, label, f, false, cx)
    }

    pub(crate) fn primary_button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        self.styled_button(id, label, f, true, cx)
    }

    fn styled_button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        primary: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let p = self.palette;
        let label = label.into();
        div()
            .id(id)
            .role(Role::Button)
            .aria_label(label.clone())
            .tab_index(0)
            .px(px(12.))
            .py(px(3.))
            .rounded(px(3.))
            .border_1()
            .border_color(if primary { p.accent } else { p.border })
            .bg(if primary { p.accent } else { p.input })
            .when(primary, |s| s.text_color(p.accent_fg))
            .cursor_pointer()
            .hover(move |s| {
                s.bg(if primary {
                    p.accent.opacity(0.86)
                } else {
                    p.selected
                })
            })
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                f(this, window, cx);
                if !this.shutting_down {
                    window.refresh();
                }
            }))
    }
    pub(crate) fn render_breadcrumbs(
        &self,
        pane: usize,
        location: &Location,
        file: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let p = self.palette;
        let bar = div()
            .id(("breadcrumbs", pane))
            .flex()
            .items_center()
            .gap(px(5.))
            .h(px(34.))
            .min_w_0()
            .flex_shrink_0()
            .px(px(12.))
            .border_b_1()
            .border_color(p.border)
            .text_size(px(12.))
            .overflow_hidden();
        let mut ancestors = div()
            .id("breadcrumb-ancestors")
            .flex()
            .items_center()
            .gap(px(5.))
            .min_w_0()
            .overflow_hidden()
            .overflow_x_scroll();
        let crumbs =
            std::iter::successors(Some(location.clone()), Location::parent).collect::<Vec<_>>();
        let depth = crumbs.len();
        let mut separator = "";
        for (index, path) in crumbs.into_iter().rev().enumerate() {
            let last = index + 1 == depth;
            let label = path.label();
            let divider = (!separator.is_empty())
                .then(|| text(separator).flex_shrink_0().text_color(p.muted));
            separator = if path
                .archive_member()
                .is_some_and(|member| member.as_os_str().is_empty())
            {
                "!/"
            } else if path.local().is_some() {
                std::path::MAIN_SEPARATOR_STR
            } else {
                "/"
            };
            if index == 0 && label.ends_with(separator) {
                separator = "";
            }
            let caption = text(label.clone())
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis();
            let crumb = if file && last {
                caption.into_any_element()
            } else {
                div()
                    .id(("crumb", index))
                    .role(Role::Button)
                    .aria_label(label)
                    .cursor_pointer()
                    .hover(move |s| s.text_color(p.accent))
                    .child(caption)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.navigate(pane, path.clone(), window, cx)
                    }))
                    .into_any_element()
            };
            if last {
                return bar
                    .when(index > 0, |bar| bar.child(ancestors))
                    .children(divider)
                    .child(
                        div()
                            .flex_shrink_0()
                            .overflow_hidden()
                            .when(index > 0, |s| s.max_w(relative(0.5)))
                            .child(crumb),
                    );
            }
            ancestors = ancestors
                .children(divider)
                .child(div().flex_shrink_0().child(crumb));
        }
        bar
    }

    fn render_pane(
        &mut self,
        pane: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let active = self.active == pane;
        let selected = self.panes[pane].active;
        let tab = self.panes[pane].tabs[selected].id();
        let scroll = &mut self.tab_strip_scrolls[pane];
        scroll.follow(selected, tab, cx.reduce_motion(), window);
        let mut tabs = div()
            .id(("pane-tabs", pane))
            .role(Role::TabList)
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .overflow_x_scroll()
            .track_scroll(&scroll.handle)
            .on_scroll(cx.listener(move |this, event: &ScrollEvent, window, _| {
                let scroll = &mut this.tab_strip_scrolls[pane];
                let delta = event.delta.pixel_delta(window.line_height());
                if scroll.handle.max_offset().x > px(0.) && (delta.x != px(0.) || delta.y != px(0.))
                {
                    scroll.manual_scroll();
                }
            }));
        for (index, tab) in self.panes[pane].tabs.iter().enumerate() {
            let id = tab.id();
            let preview = tab.preview;
            let selected = self.panes[pane].active == index;
            let (label, tab_icon) = match &tab.content {
                TabContent::Directory(browser) => {
                    let path = &browser.current().path;
                    let archive_root = matches!(path, Location::Archive { member, .. } if member.as_os_str().is_empty());
                    let label = path.label();
                    self.file_tab_label(OsStr::new(&label), !archive_root)
                }
                TabContent::Document(id) => {
                    let doc = &self.documents[id];
                    let (mut label, icon) =
                        self.file_tab_label(doc.path.file_name().unwrap_or_default(), false);
                    if doc.dirty() {
                        label.insert_str(0, "● ");
                    }
                    (label, icon)
                }
                TabContent::Hex { path, view, .. } => {
                    let (mut label, icon) =
                        self.file_tab_label(path.file_name().unwrap_or_default(), false);
                    if view.read(cx).buffer.read(cx).dirty() {
                        label.insert_str(0, "● ");
                    }
                    (label, icon)
                }
                TabContent::Image { path, .. } => {
                    self.file_tab_label(path.file_name().unwrap_or_default(), false)
                }
                TabContent::Media { path, .. } => {
                    self.file_tab_label(path.file_name().unwrap_or_default(), false)
                }
                TabContent::Panel { title, .. } => {
                    (title.clone(), TabIcon::Glyph(IconName::LayoutDashboard))
                }
                TabContent::SelectionInspector { .. } => (
                    selection_inspector::TITLE.into(),
                    TabIcon::Glyph(IconName::LayoutDashboard),
                ),
                TabContent::Pending { path, .. } => path
                    .as_ref()
                    .and_then(Location::file_name)
                    .map(|name| self.file_tab_label(name, false))
                    .unwrap_or_else(|| ("Preview".into(), TabIcon::Glyph(IconName::Eye))),
            };
            tabs = tabs.child(
                div()
                    .id(("tab", id as usize))
                    .relative()
                    .role(Role::Tab)
                    .aria_label(label.clone())
                    .aria_selected(selected)
                    .aria_description(if tab.preview {
                        "Preview tab; double-click to keep"
                    } else {
                        "Permanent tab"
                    })
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h_full()
                    .flex_shrink_0()
                    .min_w(px(64.))
                    .max_w(px(200.))
                    .px(px(8.))
                    .border_r_1()
                    .border_color(p.border)
                    .when(selected, |s| {
                        s.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .h(px(2.))
                                .bg(if active { p.accent } else { p.muted }),
                        )
                    })
                    .text_color(if selected { p.fg } else { p.muted })
                    .text_size(px(12.))
                    .cursor_pointer()
                    .on_drag(TabDrag { id }, {
                        let label = label.clone();
                        let icon = tab_icon.clone();
                        move |_, _, _, cx| {
                            cx.new(|_| TabDragPreview {
                                label: label.clone(),
                                icon: icon.clone(),
                                palette: p,
                            })
                        }
                    })
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.set_tab(pane, index, window, cx);
                        if preview && event.click_count() == 2 {
                            this.run("keep-preview", window, cx);
                        }
                    }))
                    .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.is_middle_click() {
                            cx.stop_propagation();
                            this.request_close_tab(pane, id, window, cx);
                        }
                    }))
                    .child(tab_icon.render(p))
                    .child(
                        text(label)
                            .min_w_0()
                            .when(tab.preview, |text| text.italic()),
                    )
                    .child(
                        div()
                            .id(("close-tab", id as usize))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(16.))
                            .flex_shrink_0()
                            .rounded(px(2.))
                            .hover(move |s| s.bg(p.selected))
                            .child(icon(IconName::Close, p.muted))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.request_close_tab(pane, id, window, cx);
                            })),
                    )
                    .children([false, true].map(|after| {
                        div()
                            .id(("tab-drop", usize::from(after)))
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .w(relative(0.5))
                            .when(after, |s| s.right_0())
                            .when(!after, |s| s.left_0())
                            .drag_over::<TabDrag>(move |s, _, _, _| {
                                let s = s.border_color(p.accent);
                                if after {
                                    s.border_r_2()
                                } else {
                                    s.border_l_2()
                                }
                            })
                            .on_drop(cx.listener(move |this, drag: &TabDrag, window, cx| {
                                this.drop_tab(
                                    drag.id,
                                    pane,
                                    index + usize::from(after),
                                    window.modifiers().alt,
                                    window,
                                    cx,
                                );
                            }))
                    })),
            )
        }
        let end = self.panes[pane].tabs.len();
        let tab_bar = div()
            .id(("tab-bar", pane))
            .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                let scroll = &mut this.tab_strip_scrolls[pane];
                if *hovered {
                    scroll.return_timer = None;
                } else if scroll.manually_scrolled && scroll.return_timer.is_none() {
                    scroll.return_timer = Some(cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                        let _ = cx.update(|window, cx| {
                            let _ = this.update(cx, |this, cx| {
                                this.tab_strip_scrolls[pane].return_to_active();
                                window.refresh();
                                cx.notify();
                            });
                        });
                    }));
                }
            }))
            .flex()
            .h(px(28.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(p.border)
            .child(
                tabs.child(
                    div()
                        .id(("tab-drop-end", pane))
                        .flex_1()
                        .h_full()
                        .drag_over::<TabDrag>(move |s, _, _, _| {
                            s.border_l_2().border_color(p.accent).bg(p.selected)
                        })
                        .on_drop(cx.listener(move |this, drag: &TabDrag, window, cx| {
                            this.drop_tab(drag.id, pane, end, window.modifiers().alt, window, cx);
                        })),
                ),
            )
            .child(
                div()
                    .id(("pane-menu", pane))
                    .role(Role::Button)
                    .aria_label(if pane == 0 {
                        "Left panel actions"
                    } else {
                        "Right panel actions"
                    })
                    .aria_expanded(
                        matches!(&self.modal, Some(Modal::PanelMenu(menu)) if menu.pane == pane),
                    )
                    .w(px(28.))
                    .flex_shrink_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(p.accent)
                    .text_color(p.accent_fg)
                    .cursor_pointer()
                    .child(icon(IconName::EllipsisVertical, p.accent_fg))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_panel_menu(pane, window, cx);
                    })),
            );
        let content = match &self.panes[pane].tabs[self.panes[pane].active].content {
            TabContent::Directory(_) => self.render_directory(pane, window, cx),
            TabContent::Pending { path, error, .. } => div()
                .when(active, |s| s.track_focus(&self.focus))
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_color(p.muted)
                .child(error.clone().unwrap_or_else(|| {
                    if path.is_some() {
                        "Loading preview…".into()
                    } else {
                        "Select a file to preview".into()
                    }
                }))
                .into_any_element(),
            TabContent::Document(id) => {
                let id = *id;
                let (state, readonly) = self.editor_for_pane(pane, id, window, cx);
                let doc = &self.documents[&id];
                let breadcrumbs = self.render_breadcrumbs(pane, &doc.path, true, cx);
                let status = doc.error.clone().unwrap_or_else(|| {
                    if doc.saving {
                        "Saving…".into()
                    } else if doc.readonly {
                        "Read only".into()
                    } else if doc.dirty() {
                        "Modified · F2 / ⌘S to save".into()
                    } else {
                        "Saved".into()
                    }
                });
                let status = if let Some(language) = &doc.lsp_status {
                    format!("{status} · {language}")
                } else {
                    status
                };
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(breadcrumbs)
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .font_family("Menlo")
                            .when(!readonly, |editor| {
                                editor
                                    .capture_action(cx.listener(move |this, _: &Undo, _, cx| {
                                        this.reconcile_after_history(id, cx);
                                    }))
                                    .capture_action(cx.listener(move |this, _: &Redo, _, cx| {
                                        this.reconcile_after_history(id, cx);
                                    }))
                            })
                            .child(
                                Editor::new(&state)
                                    .h(relative(1.))
                                    .appearance(false)
                                    .bordered(false)
                                    .readonly(readonly),
                            ),
                    )
                    .child(
                        div()
                            .h(px(26.))
                            .flex_shrink_0()
                            .px(px(12.))
                            .py(px(4.))
                            .border_t_1()
                            .border_color(p.border)
                            .text_size(px(12.))
                            .text_color(if doc.error.is_some() {
                                p.error
                            } else {
                                p.muted
                            })
                            .child(status),
                    )
                    .into_any_element()
            }
            TabContent::Hex { path, view, .. } => {
                view.update(cx, |view, _| view.palette = p);
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_breadcrumbs(pane, path, true, cx))
                    .child(div().flex_1().min_h_0().child(view.clone()))
                    .into_any_element()
            }
            TabContent::Media { path, view, .. } => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(self.render_breadcrumbs(pane, path, true, cx))
                .child(div().flex_1().min_h_0().child(view.clone()))
                .into_any_element(),
            TabContent::Image { path, image, .. } => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .when(active, |s| s.track_focus(&self.focus))
                .key_context("PreviewPane")
                .child(self.render_breadcrumbs(pane, path, true, cx))
                .child(
                    div().flex_1().min_h_0().overflow_hidden().child(
                        if let Some(image) = image {
                            img(image.clone())
                        } else {
                            img(path.host_path().expect("local image path").to_path_buf())
                        }
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                    ),
                )
                .into_any_element(),
            TabContent::SelectionInspector { snapshot, .. } => div()
                .id(("selection-inspector", pane))
                .when(active, |s| s.track_focus(&self.focus))
                .key_context("ExtensionPane")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .gap(px(12.))
                .p(px(16.))
                .child(text(selection_inspector::TITLE))
                .child(text(snapshot.path.clone()).text_color(p.muted))
                .child(
                    self.render_panel_table(
                        ["Name", "Kind", "Size"].map(str::to_owned).into(),
                        snapshot
                            .entries
                            .iter()
                            .map(|entry| {
                                vec![
                                    entry.name.clone(),
                                    if entry.directory { "Directory" } else { "File" }.into(),
                                    entry.size.to_string(),
                                ]
                            })
                            .collect(),
                    ),
                )
                .child(self.button(
                    "refresh-selection-inspector",
                    "Refresh selection",
                    move |this, _, cx| this.refresh_selection_inspector(pane, cx),
                    cx,
                ))
                .into_any_element(),
            TabContent::Panel {
                node,
                extension,
                command,
                title,
                ..
            } => {
                let node = node.clone();
                let (extension, command, title) =
                    (extension.clone(), command.clone(), title.clone());
                div()
                    .id(("extension-panel", pane))
                    .when(active, |s| s.track_focus(&self.focus))
                    .key_context("ExtensionPane")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(px(16.))
                    .child(self.render_extension_node(node, extension, command, title, cx))
                    .into_any_element()
            }
        };
        div()
            .when(
                self.panes[pane].tabs[self.panes[pane].active].preview,
                |s| s.key_context("PreviewTab"),
            )
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .border_1()
            .border_color(if active { p.accent } else { p.border })
            .bg(p.bg)
            .child(tab_bar)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .capture_any_mouse_down(cx.listener(
                        move |this, event: &MouseDownEvent, window, cx| {
                            this.focus_pane_from_click(pane, event, window, cx);
                        },
                    ))
                    .on_drop(cx.listener(move |this, drag: &TabDrag, window, cx| {
                        if this.panes[pane].tabs.iter().all(|tab| tab.id() != drag.id) {
                            this.drop_tab(drag.id, pane, end, window.modifiers().alt, window, cx);
                        }
                    }))
                    .child(content),
            )
            .into_any_element()
    }

    fn render_extension_node(
        &self,
        node: PanelNode,
        extension: String,
        command: String,
        title: String,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let row = matches!(node, PanelNode::Row { .. });
        match node {
            PanelNode::Column { children } | PanelNode::Row { children } => {
                let mut view = div().flex().gap(px(12.)).when(!row, |s| s.flex_col());
                for child in children {
                    view = view.child(self.render_extension_node(
                        child,
                        extension.clone(),
                        command.clone(),
                        title.clone(),
                        cx,
                    ));
                }
                view.into_any_element()
            }
            PanelNode::Text { text: value, muted } => text(value)
                .text_color(if muted { p.muted } else { p.fg })
                .into_any_element(),
            PanelNode::Button { id, text: value } => self
                .button(
                    SharedString::from(format!("ext-{id}")),
                    value,
                    move |this, _, cx| {
                        this.invoke_extension(
                            extension.clone(),
                            command.clone(),
                            title.clone(),
                            json!({"id":id}),
                            cx,
                        )
                    },
                    cx,
                )
                .into_any_element(),
            PanelNode::Table { columns, rows } => {
                self.render_panel_table(columns, rows).into_any_element()
            }
        }
    }

    fn render_panel_table(&self, columns: Vec<String>, rows: Vec<Vec<String>>) -> Div {
        let p = self.palette;
        let header = div()
            .flex()
            .bg(p.input)
            .children(columns.into_iter().map(|column| {
                text(column)
                    .flex_1()
                    .p(px(8.))
                    .font_weight(FontWeight::SEMIBOLD)
            }));
        div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(p.border)
            .child(header)
            .children(rows.into_iter().enumerate().map(|(index, row)| {
                div()
                    .flex()
                    .when(index % 2 == 0, |s| s.bg(p.selected))
                    .children(row.into_iter().map(|cell| text(cell).flex_1().p(px(8.))))
            }))
    }

    fn render_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let width: f32 = window.viewport_size().width.into();
        let height = if self.panels_visible {
            self.settings.terminal_height
        } else {
            f32::from(window.viewport_size().height) - 104.
        };
        let mut body = div()
            .id("terminal")
            .key_context("TerminalPane")
            .track_focus(&self.terminal_focus)
            .h(px(height))
            .flex_shrink_0()
            .overflow_hidden()
            .bg(rgb(0x171b24))
            .font_family("Menlo")
            .text_size(px(13.))
            .line_height(px(18.))
            .px(px(8.))
            .py(px(4.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.terminal_focus.focus(window, cx)),
            )
            .on_key_down(cx.listener(Self::terminal_key))
            .on_scroll(cx.listener(|this, e: &ScrollEvent, _, cx| {
                if let Some(t) = this.terminals.get_mut(this.terminal_active) {
                    let delta: f32 = e.delta.pixel_delta(px(18.)).y.into();
                    t.scroll((delta / 18.).round() as i32);
                    cx.notify();
                }
            }));
        if let Some(t) = self.terminals.get_mut(self.terminal_active) {
            t.resize(
                ((width - 16.) / 7.83) as usize,
                ((height - 8.) / 18.) as usize,
            );
            let rows = t.snapshot();
            for runs in rows {
                let mut line = String::new();
                let mut highlights = vec![];
                for run in runs {
                    let start = line.len();
                    line.push_str(&run.text);
                    highlights.push((
                        start..line.len(),
                        HighlightStyle {
                            color: Some(
                                rgb(if run.cursor {
                                    0x171b24
                                } else {
                                    terminal::color(run.fg, true)
                                })
                                .into(),
                            ),
                            background_color: Some(
                                rgb(if run.cursor {
                                    0xffcc66
                                } else {
                                    terminal::color(run.bg, false)
                                })
                                .into(),
                            ),
                            ..Default::default()
                        },
                    ));
                }
                body = body.child(
                    div()
                        .h(px(18.))
                        .child(StyledText::new(line).with_highlights(highlights)),
                );
            }
        } else {
            body = body.child(text("No shell running").text_color(p.muted));
        }
        body.into_any_element()
    }

    fn transfer_dropdown<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        label: &'static str,
        selected: T,
        choices: Vec<(&'static str, T)>,
        set: fn(&mut Self, T),
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let title = choices
            .iter()
            .find_map(|(name, value)| (*value == selected).then_some(*name))
            .unwrap_or(choices[0].0);
        let view = cx.weak_entity();
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .child(label)
            .child(
                Button::new(id)
                    .accessibility_label(format!("{label}, {title}"))
                    .label(title)
                    .icon(IconName::ChevronDown)
                    .dropdown_menu({
                        let view = view.clone();
                        move |mut menu, _, _| {
                            for &(label, value) in &choices {
                                let view = view.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(label)
                                        .checked(value == selected)
                                        .on_click(move |_, _, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                set(this, value);
                                                cx.notify();
                                            });
                                        }),
                                );
                            }
                            menu
                        }
                    })
                    .on_open_change(move |open, _, cx| {
                        let _ = view.update(cx, |this, _| {
                            if *open {
                                this.transfer_dropdown_open = Some(id);
                            } else if this.transfer_dropdown_open == Some(id) {
                                this.transfer_dropdown_open = None;
                            }
                        });
                    }),
            )
    }

    fn render_transfer_options(
        &self,
        kind: OperationKind,
        sources: &[Location],
        archive_destination: Option<&Location>,
        cx: &Context<Self>,
    ) -> Div {
        let p = self.palette;
        let value = self.dialog_input.read(cx).value();
        let archive_target = !self.copy_multiple
            && archive_destination.is_some_and(|path| path.display() == value.trim());
        let local_copy = matches!(kind, OperationKind::Copy)
            && !archive_target
            && sources.iter().all(|source| source.local().is_some());
        let options = &self.copy_options;
        let choices = [
            ("Ask", ConflictPolicy::Ask),
            ("Overwrite", ConflictPolicy::Overwrite),
            ("Skip", ConflictPolicy::Skip),
            ("Rename", ConflictPolicy::Rename),
            ("Append", ConflictPolicy::Append),
            ("Resume", ConflictPolicy::Resume),
            ("Only newer files", ConflictPolicy::OnlyNewer),
        ]
        .into_iter()
        .filter(|(_, policy)| self.transfer_supports_policy(kind, sources, archive_target, *policy))
        .collect();
        let mut form = div().flex().flex_col().gap(px(10.))
            .child(div().flex().flex_col().gap(px(4.))
                .child(text("Destination").text_color(p.muted))
                .child(div().flex().items_center().gap(px(8.))
                    .child(div().flex_1().min_w_0().child(
                        Input::new(&self.dialog_input).aria_label("Destination")
                    ))
                    .child(self.button("transfer-browse", "Browse… (F10)", |this, window, cx| {
                        this.choose_transfer_destination(window, cx);
                    }, cx))
                )
                .when(self.copy_multiple, |s| s.child(text("Separate paths with semicolons. Quote a path that contains a semicolon.")
                    .text_color(p.muted).text_size(px(12.))))
            )
            .child(self.transfer_dropdown("transfer-policy", "Existing files", options.conflict_policy, choices,
                |this, policy| this.copy_options.conflict_policy = policy, cx));
        if !local_copy {
            return form;
        }
        form = form.child(self.transfer_dropdown(
            "copy-symlinks",
            "Symbolic links",
            options.symlink_mode,
            vec![
                ("Always copy link", SymlinkMode::AlwaysLink),
                ("Smart", SymlinkMode::Smart),
                ("Always copy target", SymlinkMode::AlwaysTarget),
            ],
            |this, mode| this.copy_options.symlink_mode = mode,
            cx,
        ));
        if options.symlink_mode == SymlinkMode::Smart {
            form = form.child(
                text("Keep links within the selection and copy the contents of external targets.")
                    .text_color(p.muted)
                    .text_size(px(12.)),
            );
        }
        form.child(
            div()
                .flex()
                .gap(px(24.))
                .border_t_1()
                .border_color(p.border)
                .pt(px(10.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap(px(7.))
                        .child(
                            Checkbox::new("copy-multiple")
                                .label("Multiple destinations")
                                .checked(self.copy_multiple)
                                .on_change(cx.listener(|this, checked: &bool, window, cx| {
                                    this.set_copy_multiple(*checked, window, cx);
                                })),
                        )
                        .child(
                            Checkbox::new("copy-read-only")
                                .label("Also ask about read-only files")
                                .checked(options.ask_read_only)
                                .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                    this.copy_options.ask_read_only = *checked;
                                    cx.notify();
                                })),
                        )
                        .when(cfg!(unix), |s| {
                            s.child(
                                Checkbox::new("copy-permissions")
                                    .label("Copy permissions")
                                    .checked(options.copy_permissions)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        this.copy_options.copy_permissions = *checked;
                                        cx.notify();
                                    })),
                            )
                        })
                        .when(cfg!(unix), |s| {
                            s.child(
                                Checkbox::new("copy-xattrs")
                                    .label("Copy extended attributes")
                                    .checked(options.copy_xattrs)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        this.copy_options.copy_xattrs = *checked;
                                        cx.notify();
                                    })),
                            )
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap(px(7.))
                        .when(cfg!(target_os = "macos"), |s| {
                            s.child(
                                Checkbox::new("copy-write-cache")
                                    .label("Disable write cache")
                                    .checked(options.disable_write_cache)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        this.copy_options.disable_write_cache = *checked;
                                        cx.notify();
                                    })),
                            )
                        })
                        .when(cfg!(unix), |s| {
                            s.child(
                                Checkbox::new("copy-sparse")
                                    .label("Produce sparse files")
                                    .checked(options.sparse_files)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        this.copy_options.sparse_files = *checked;
                                        cx.notify();
                                    })),
                            )
                        })
                        .when(cfg!(any(target_os = "macos", target_os = "linux")), |s| {
                            s.child(
                                Checkbox::new("copy-cow")
                                    .label("Use copy-on-write if possible")
                                    .checked(options.use_cow)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        this.copy_options.use_cow = *checked;
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(
                            Checkbox::new("copy-filter-enabled")
                                .label("Use file filter")
                                .checked(self.copy_use_filter)
                                .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                    this.copy_use_filter = *checked;
                                    this.modal_error = None;
                                    cx.notify();
                                })),
                        ),
                ),
        )
        .when(self.copy_use_filter, |s| {
            s.child(Input::new(&self.copy_filter).aria_label("File filter"))
        })
    }

    fn render_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let Some(modal) = &self.modal else {
            return div().into_any_element();
        };
        if let Modal::PanelMenu(menu) = modal {
            return self.render_panel_menu(menu, window, cx);
        }
        let (title, description) = match modal {
            Modal::Prompt {
                title, description, ..
            } => (title.clone(), description.clone()),
            Modal::Palette => ("Commands".into(), String::new()),
            Modal::CreateArchive(dialog) => (
                "Create Archive".into(),
                format!("{} selected item(s)", dialog.selection_count()),
            ),
            Modal::ArchivePassword { request, .. } => (
                "Unlock Archive".into(),
                if request.member.as_os_str().is_empty() {
                    request.archive.clone()
                } else {
                    format!("{}\n{}", request.archive, request.member.display())
                },
            ),
            Modal::QuickPick { request, .. } => (
                request
                    .title
                    .clone()
                    .unwrap_or_else(|| "Select an option".into()),
                request.place_holder.clone().unwrap_or_default(),
            ),
            Modal::PanelMenu(_) => unreachable!(),
            Modal::Settings => ("Settings".into(), String::new()),
            Modal::Extensions => ("Extensions".into(), String::new()),
            Modal::OpenVsx => ("Extensions".into(), String::new()),
            Modal::ExtensionLogs => ("Extension Logs".into(), String::new()),
            Modal::Search => (
                "Find Files".into(),
                self.cwd(self.active).display().to_string(),
            ),
            Modal::Conflict {
                source,
                destination,
                ..
            } => (
                "File already exists".into(),
                format!("{}\n→ {}", source.display(), destination.display()),
            ),
        };
        let marketplace = matches!(modal, Modal::OpenVsx);
        let mut content = div()
            .id("modal-content")
            .track_focus(&self.modal_focus)
            .key_context("Modal")
            .role(Role::Dialog)
            .aria_label(title.clone())
            .tab_group()
            .flex()
            .flex_col()
            .gap(px(12.))
            .w(px(if marketplace { 1100. } else { 620. }))
            .max_w(relative(if marketplace { 0.95 } else { 0.9 }))
            .max_h(relative(0.82))
            .when(!marketplace, |s| s.overflow_y_scroll())
            .p(px(20.))
            .rounded(px(7.))
            .bg(p.bg)
            .border_1()
            .border_color(p.border)
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        text(title)
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .flex_1(),
                    )
                    .child(self.button(
                        "close-modal",
                        "×",
                        |this, window, cx| this.dismiss(window, cx),
                        cx,
                    )),
            );
        if !description.is_empty() {
            content = content.child(
                div()
                    .text_size(px(12.))
                    .text_color(p.muted)
                    .child(description),
            );
        }
        match modal {
            Modal::PanelMenu(_) => unreachable!(),
            Modal::ArchivePassword { request, input } => {
                if request.incorrect {
                    content =
                        content.child(text("Incorrect password. Try again.").text_color(p.error));
                }
                content = content
                    .child(Input::new(input).aria_label("Archive password"))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(self.button(
                                "password-cancel",
                                "Cancel",
                                |this, window, cx| this.dismiss(window, cx),
                                cx,
                            ))
                            .child(
                                self.button(
                                    "password-unlock",
                                    "Unlock",
                                    |this, window, cx| this.accept_modal(window, cx),
                                    cx,
                                )
                                .bg(p.accent)
                                .text_color(p.accent_fg),
                            ),
                    );
            }
            Modal::CreateArchive(dialog) => {
                use crate::app::ArchiveOption;
                use dotdir_archive_api::OptionKind;

                let mut formats = div().flex().flex_wrap().gap(px(6.));
                for format in crate::archive_extensions::BUNDLED.creators() {
                    let selected = dialog.format.info().id == format.info().id;
                    formats = formats.child(
                        self.button(
                            format.info().id,
                            format.creation().unwrap().name,
                            move |this, window, cx| this.select_archive_format(format, window, cx),
                            cx,
                        )
                        .when(selected, |button| button.border_color(p.accent)),
                    );
                }
                content = content
                    .child(text("Format"))
                    .child(formats)
                    .child(text("Output file"))
                    .child(Input::new(&self.dialog_input).aria_label("Output file"));
                let schema = dialog.format.creation().unwrap();
                for (index, (field, value)) in schema.fields.iter().zip(&dialog.options).enumerate()
                {
                    content = content.child(text(field.label));
                    match (value, &field.kind) {
                        (ArchiveOption::Choice(selected), OptionKind::Choice(choices)) => {
                            let mut buttons = div().flex().flex_wrap().gap(px(6.));
                            for choice in *choices {
                                let value = choice.value;
                                buttons = buttons.child(
                                    self.button(
                                        SharedString::from(format!("archive-{}-{value}", field.id)),
                                        choice.label,
                                        move |this, window, cx| {
                                            this.select_archive_option(index, value, window, cx)
                                        },
                                        cx,
                                    )
                                    .when(*selected == value, |button| {
                                        button.border_color(p.accent)
                                    }),
                                );
                            }
                            content = content.child(buttons);
                        }
                        (ArchiveOption::Text(input), _) => {
                            content = content.child(Input::new(input).aria_label(field.label));
                        }
                        _ => {}
                    }
                }
                content = content.child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(self.button(
                            "cancel",
                            "Cancel",
                            |this, window, cx| this.dismiss(window, cx),
                            cx,
                        ))
                        .child(
                            self.button(
                                "create",
                                "Create Archive",
                                |this, window, cx| this.accept_modal(window, cx),
                                cx,
                            )
                            .bg(p.accent)
                            .text_color(p.accent_fg),
                        ),
                );
            }
            Modal::Palette => {
                content = content.child(Input::new(&self.dialog_input));
                let commands = self.filtered_commands(cx);
                self.palette_cursor = self.palette_cursor.min(commands.len().saturating_sub(1));
                let start = self.palette_cursor.saturating_sub(10);
                let mut list = div().flex().flex_col().gap(px(2.));
                if commands.is_empty() {
                    list = list.child(text("No available commands").text_color(p.muted));
                }
                for (index, (id, title, key)) in
                    commands.into_iter().enumerate().skip(start).take(12)
                {
                    let selected = index == self.palette_cursor;
                    list = list.child(
                        div()
                            .id(("command", index))
                            .role(Role::Button)
                            .aria_label(title.clone())
                            .flex()
                            .items_center()
                            .h(px(32.))
                            .px(px(8.))
                            .rounded(px(3.))
                            .when(selected, |s| {
                                s.bg(p.selected)
                                    .when_some(p.selected_fg, |style, fg| style.text_color(fg))
                            })
                            .cursor_pointer()
                            .hover(move |s| s.bg(p.selected))
                            .child(text(title).flex_1())
                            .child(
                                text(key.replace("cmd-", "⌘"))
                                    .text_color(if selected {
                                        p.selected_fg.unwrap_or(p.accent)
                                    } else {
                                        p.accent
                                    })
                                    .text_size(px(11.)),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.dismiss(window, cx);
                                this.run(&id, window, cx);
                            })),
                    );
                }
                content = content.child(list);
            }
            Modal::QuickPick { request, .. } => {
                content = content.child(Input::new(&self.dialog_input));
                let indices = request.matches(&self.dialog_input.read(cx).value());
                self.palette_cursor = self.palette_cursor.min(indices.len().saturating_sub(1));
                let start = self.palette_cursor.saturating_sub(7);
                let mut list = div().flex().flex_col().gap(px(2.));
                if indices.is_empty() {
                    list = list.child(text("No matching options").text_color(p.muted));
                }
                for (cursor, &index) in indices.iter().enumerate().skip(start).take(9) {
                    let group_start = cursor.checked_sub(1).map_or(0, |prev| indices[prev] + 1);
                    if let Some(separator) = request.items[group_start..index]
                        .iter()
                        .rev()
                        .find(|item| item.separator)
                    {
                        list = list.child(
                            div()
                                .border_t_1()
                                .border_color(p.border)
                                .px(px(8.))
                                .text_size(px(11.))
                                .text_color(p.muted)
                                .child(separator.label.clone()),
                        );
                    }
                    let item = &request.items[index];
                    let mut heading = div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(text(item.label.clone()));
                    if let Some(description) = &item.description {
                        heading = heading.child(text(description.clone()).text_color(p.muted));
                    }
                    let mut row = div()
                        .id(("quick-pick", index))
                        .role(Role::Button)
                        .aria_label(item.label.clone())
                        .flex()
                        .flex_col()
                        .px(px(8.))
                        .py(px(6.))
                        .rounded(px(3.))
                        .when(cursor == self.palette_cursor, |s| {
                            s.bg(p.selected)
                                .when_some(p.selected_fg, |style, fg| style.text_color(fg))
                        })
                        .cursor_pointer()
                        .hover(move |s| s.bg(p.selected))
                        .child(heading)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.accept_quick_pick(index, window, cx);
                        }));
                    if let Some(detail) = &item.detail {
                        row =
                            row.child(text(detail.clone()).text_size(px(11.)).text_color(p.muted));
                    }
                    list = list.child(row);
                }
                content = content.child(list);
            }
            Modal::Prompt { kind, .. } => {
                if let PromptKind::Transfer {
                    kind,
                    sources,
                    archive_destination,
                } = kind
                {
                    content = content.child(self.render_transfer_options(
                        *kind,
                        sources,
                        archive_destination.as_ref(),
                        cx,
                    ));
                } else if !matches!(
                    kind,
                    PromptKind::Trash(_)
                        | PromptKind::DeleteArchive(_)
                        | PromptKind::CloseTab { .. }
                        | PromptKind::Quit
                ) {
                    content = content.child(Input::new(&self.dialog_input));
                }
                let confirm = match kind {
                    PromptKind::Trash(_) => "Move to Trash",
                    PromptKind::DeleteArchive(_) => "Delete Entries",
                    PromptKind::CloseTab { .. } => "Discard Changes",
                    PromptKind::Quit => "Close Without Saving",
                    PromptKind::Transfer { kind, .. } => kind.title(),
                    _ => "OK",
                };
                content = content.child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(self.button(
                            "cancel",
                            "Cancel",
                            |this, window, cx| this.dismiss(window, cx),
                            cx,
                        ))
                        .child(
                            self.button(
                                "confirm",
                                confirm,
                                |this, window, cx| this.accept_modal(window, cx),
                                cx,
                            )
                            .bg(p.accent)
                            .text_color(p.accent_fg),
                        ),
                );
            }
            Modal::Settings => {
                content = content.child(self.icon_theme_controls("settings-icons", cx));
                content = content.child(text("Appearance").font_weight(FontWeight::SEMIBOLD));
                let mut themes = div().flex().gap(px(8.));
                for (id, label) in [
                    ("ayu", "Ayu Mirage"),
                    ("mocha", "Catppuccin Mocha"),
                    ("light", "Catppuccin Latte"),
                ] {
                    themes = themes.child(self.button(
                        SharedString::from(id),
                        format!(
                            "{}{}",
                            if self.settings.theme == id && self.color_theme_name.is_none() {
                                "✓ "
                            } else {
                                ""
                            },
                            label
                        ),
                        move |this, _, cx| this.use_builtin_theme(id, cx),
                        cx,
                    ));
                }
                content = content.child(themes);
                if let Some(name) = &self.color_theme_name {
                    content = content.child(text(name.clone()));
                }
                content = content.child(self.button(
                    "load-vscode-color-theme",
                    "Load VS Code Color Theme…",
                    |this, window, cx| this.prompt_color_theme(window, cx),
                    cx,
                ));
                content = content.child(self.button(
                    "hidden",
                    format!(
                        "{} Show hidden files",
                        if self.settings.show_hidden {
                            "☑"
                        } else {
                            "☐"
                        }
                    ),
                    |this, _, cx| {
                        this.settings.show_hidden = !this.settings.show_hidden;
                        this.refresh_all();
                        this.persist();
                        cx.notify();
                    },
                    cx,
                ));
                content = content
                    .child(text("Active pane presentation"))
                    .child(self.panel_view_selector(self.active, cx));
                content = content.child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(text(format!("Font size: {} px", self.settings.font_size)).flex_1())
                        .child(self.button(
                            "font-minus",
                            "−",
                            |this, _, cx| {
                                this.settings.font_size = (this.settings.font_size - 1.).max(10.);
                                this.persist();
                                cx.notify();
                            },
                            cx,
                        ))
                        .child(self.button(
                            "font-plus",
                            "+",
                            |this, _, cx| {
                                this.settings.font_size = (this.settings.font_size + 1.).min(24.);
                                this.persist();
                                cx.notify();
                            },
                            cx,
                        )),
                );
                let minutes = self.settings.archive_password_cache_minutes;
                let label = if minutes == 0 {
                    "Archive password cache · Off".into()
                } else {
                    format!("Archive password cache · {minutes} min")
                };
                content = content.child(self.button("archive-cache", label, |this, window, cx| {
                    this.prompt(
                        PromptKind::ArchiveCache,
                        "Archive Password Cache",
                        "Enter how many minutes to remember a password, or 0 to ask on the next operation.".into(),
                        this.settings.archive_password_cache_minutes.to_string(),
                        window, cx,
                    );
                }, cx));
                content=content.child(self.button("keybindings","Edit keybindings.json",|this,window,cx|{let path=this.settings_path.with_file_name("keybindings.json");if !path.exists(){let _=std::fs::write(&path,"[\n  {\"key\": \"cmd-l\", \"command\": \"location\", \"context\": \"DotDir\"}\n]\n");}this.dismiss(window,cx);this.open_document(this.active,path,false,window,cx);},cx));
            }
            Modal::OpenVsx => {
                content = content.child(self.render_marketplace(window, cx));
            }
            Modal::Extensions => {
                let install_vsix = match self.market.installing.as_ref() {
                    None => self
                        .button(
                            "install-vsix",
                            "Install from VSIX…",
                            |this, window, cx| this.choose_vsix(window, cx),
                            cx,
                        )
                        .into_any_element(),
                    Some(crate::open_vsx::InstallSource::Vsix) => text("Installing VSIX…")
                        .text_color(p.muted)
                        .into_any_element(),
                    Some(_) => text("Another installation is running")
                        .text_color(p.muted)
                        .into_any_element(),
                };
                content = content.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(8.))
                        .child(
                            self.button(
                                "browse-open-vsx",
                                "Browse Open VSX…",
                                |this, window, cx| this.open_marketplace(window, cx),
                                cx,
                            )
                            .border_color(p.accent),
                        )
                        .child(install_vsix)
                        .child(self.button(
                            "extension-logs",
                            "Extension Logs",
                            |this, window, cx| {
                                this.extension_log_scroll.scroll_to_bottom();
                                this.show_modal(Modal::ExtensionLogs, window, cx);
                            },
                            cx,
                        ))
                        .child(self.button(
                            "browse-logs",
                            "Browse Logs",
                            |this, window, cx| this.browse_extension_logs(window, cx),
                            cx,
                        ))
                        .child(self.button(
                            "open-dotdir-log",
                            "Open DotDir Log",
                            |this, window, cx| {
                                let path = this
                                    .settings_path
                                    .with_file_name(crate::extension_logs::ERROR_LOG_NAME);
                                this.dismiss(window, cx);
                                this.open_document(this.active, path, true, window, cx);
                            },
                            cx,
                        )),
                );
                content = content.when_some(self.market.vsix_result.as_ref(), |content, result| {
                    content.child(match result {
                        Ok(message) => text(message.clone()).text_color(p.accent),
                        Err(message) => text(message.clone()).text_color(p.error),
                    })
                });
                content = content.child(self.icon_theme_controls("extension-icons", cx));
                for format in crate::archive_extensions::BUNDLED.iter() {
                    let suffixes = format
                        .suffixes
                        .iter()
                        .map(|suffix| format!(".{suffix}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    content = content.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .p(px(12.))
                            .border_1()
                            .border_color(p.border)
                            .rounded(px(4.))
                            .child(
                                text(format!("{}  {}", format.name, format.version))
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                text(format!("Bundled archive extension · {suffixes}"))
                                    .text_color(p.muted),
                            ),
                    );
                }
                for (title, actions) in [
                    (VIEWER_TITLE, &[("View selected file", "view")][..]),
                    (
                        selection_inspector::TITLE,
                        &[
                            (selection_inspector::TITLE, selection_inspector::INSPECT),
                            (
                                selection_inspector::SUMMARY_TITLE,
                                selection_inspector::SUMMARY,
                            ),
                        ][..],
                    ),
                ] {
                    let mut row = div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .p(px(12.))
                        .border_1()
                        .border_color(p.border)
                        .rounded(px(4.))
                        .child(text(title).font_weight(FontWeight::SEMIBOLD))
                        .child(text("Bundled extension").text_color(p.muted));
                    for &(label, command) in actions {
                        row = row.child(self.button(
                            SharedString::from(format!("bundled-{command}")),
                            label,
                            move |this, window, cx| {
                                this.dismiss(window, cx);
                                this.run(command, window, cx);
                            },
                            cx,
                        ));
                    }
                    content = content.child(row);
                }
                for e in &self.extensions {
                    let mut row = div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .p(px(12.))
                        .border_1()
                        .border_color(p.border)
                        .rounded(px(4.))
                        .child(
                            text(format!("{}  {}", e.manifest.name, e.manifest.version))
                                .font_weight(FontWeight::SEMIBOLD),
                        );
                    for c in &e.contributions {
                        if let Some(viewer) = &c.viewer {
                            row = row
                                .child(
                                    text(format!(
                                        "{} · {} file patterns",
                                        c.title,
                                        viewer.patterns.len()
                                    ))
                                    .text_color(p.muted),
                                )
                                .child(self.button(
                                    SharedString::from(format!("viewer-{}", c.id)),
                                    "View selected file",
                                    |this, window, cx| {
                                        this.dismiss(window, cx);
                                        this.run("view", window, cx);
                                    },
                                    cx,
                                ));
                            continue;
                        }
                        let (extension, command, title) =
                            (e.manifest.id.clone(), c.id.clone(), c.title.clone());
                        row = row.child(self.button(
                            SharedString::from(format!("extension-{}", c.id)),
                            c.title.clone(),
                            move |this, window, cx| {
                                this.dismiss(window, cx);
                                this.invoke_extension(
                                    extension.clone(),
                                    command.clone(),
                                    title.clone(),
                                    json!(null),
                                    cx,
                                );
                            },
                            cx,
                        ));
                    }
                    content = content.child(row);
                }
                for extension in &self.vscode_extensions {
                    let id = extension.id.clone();
                    let enabled = self.settings.vscode.enabled(&id);
                    content = content.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .p(px(12.))
                            .border_1()
                            .border_color(p.border)
                            .rounded(px(4.))
                            .child(
                                div()
                                    .flex_1()
                                    .child(text(format!(
                                        "{}  {}",
                                        extension.title, extension.version
                                    )))
                                    .child(text(format!("VS Code · {id}")).text_color(p.muted)),
                            )
                            .child(self.button(
                                SharedString::from(format!("vscode-extension-{id}")),
                                if enabled { "Disable" } else { "Enable" },
                                move |this, _, cx| this.toggle_vscode_extension(&id, cx),
                                cx,
                            )),
                    );
                }
                for error in &self.extension_errors {
                    content = content.child(text(error.clone()).text_color(p.error));
                }
                content = content.child(self.button(
                    "reload-extensions",
                    "Reload Extensions",
                    |this, _, cx| {
                        this.extension_errors.clear();
                        this.load_extensions();
                        this.refresh_provider_directories();
                        this.refresh_vscode_language_servers(true, cx);
                        cx.notify();
                    },
                    cx,
                ));
            }
            Modal::ExtensionLogs => {
                let log = self.extension_logs.snapshot();
                let output = if log.is_empty() {
                    text("No extension output yet").text_color(p.muted)
                } else {
                    div().whitespace_normal().child(SharedString::from(log))
                };
                content = content
                    .child(
                        div()
                            .id("extension-log-output")
                            .track_scroll(&self.extension_log_scroll)
                            .overflow_y_scroll()
                            .h(px(400.))
                            .max_h(window.viewport_size().height * 0.55)
                            .p(px(10.))
                            .border_1()
                            .border_color(p.border)
                            .rounded(px(4.))
                            .font_family("Menlo")
                            .text_size(px(11.))
                            .child(output),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(self.button(
                                "extension-log-back",
                                "Back to Extensions",
                                |this, window, cx| this.show_modal(Modal::Extensions, window, cx),
                                cx,
                            ))
                            .child(self.button(
                                "copy-extension-log",
                                "Copy Logs",
                                |this, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        this.extension_logs.snapshot(),
                                    ));
                                },
                                cx,
                            )),
                    );
            }
            Modal::Search => {
                content = content
                    .child(Input::new(&self.dialog_input))
                    .child(Input::new(&self.search_content));
                content = content.child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(self.button(
                            "search",
                            "Find",
                            |this, window, cx| this.begin_search(window, cx),
                            cx,
                        ))
                        .child(self.button(
                            "stop-search",
                            "Stop",
                            |this, _, cx| {
                                if let Some(s) = &this.search {
                                    s.token.cancel();
                                }
                                cx.notify();
                            },
                            cx,
                        )),
                );
                if let Some(s) = &self.search {
                    content = content.child(
                        text(s.error.clone().unwrap_or_else(|| {
                            format!(
                                "{} matches{}",
                                s.count,
                                if s.done { "" } else { " · searching…" }
                            )
                        }))
                        .text_color(p.muted),
                    );
                    let mut list = div()
                        .id("search-results")
                        .max_h(px(300.))
                        .overflow_y_scroll();
                    for (i, path) in s.results.iter().enumerate().take(10000) {
                        list = list.child(
                            div()
                                .id(("result", i))
                                .role(Role::ListBoxOption)
                                .aria_label(path.display().to_string())
                                .aria_selected(i == s.cursor)
                                .when(i == s.cursor, |v| v.bg(p.selected))
                                .px(px(6.))
                                .py(px(4.))
                                .cursor_pointer()
                                .hover(move |s| s.bg(p.selected))
                                .child(text(path.display().to_string()))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_search_result(i, window, cx)
                                })),
                        );
                    }
                    content = content.child(list);
                }
            }
            Modal::Conflict { .. } => {
                let mut buttons = div().flex().flex_wrap().gap(px(8.));
                for (i, (label, resolution)) in [
                    ("Overwrite", ConflictResolution::Overwrite),
                    ("Skip", ConflictResolution::Skip),
                    ("Overwrite All", ConflictResolution::OverwriteAll),
                    ("Skip All", ConflictResolution::SkipAll),
                    ("Cancel", ConflictResolution::Cancel),
                ]
                .into_iter()
                .enumerate()
                {
                    buttons = buttons.child(
                        self.button(
                            ("conflict", i),
                            label,
                            move |this, window, cx| {
                                if let Some(Modal::Conflict { reply, .. }) = this.modal.take() {
                                    let _ = reply.send(resolution.clone());
                                }
                                this.focus_pane(this.active, window, cx);
                            },
                            cx,
                        )
                        .when(i == self.palette_cursor, |s| s.border_color(p.accent)),
                    );
                }
                content = content.child(buttons);
            }
        }
        if let Some(error) = &self.modal_error {
            content = content.child(div().text_color(p.error).child(error.clone()));
        }
        let height: f32 = window.viewport_size().height.into();
        div()
            .id("modal-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .justify_center()
            .items_start()
            .pt(px((height * 0.12).min(80.)))
            .bg(gpui_kit::rgba(0x00000070))
            .child(content)
            .into_any_element()
    }
}

impl Render for DotDir {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for pane in &self.panes {
            for (i, tab) in pane.tabs.iter().enumerate() {
                if let TabContent::Media { view, .. } = &tab.content {
                    view.update(cx, |view, _| {
                        view.set_visible(self.panels_visible && pane.active == i)
                    });
                }
            }
        }
        let p = self.palette;
        let mut root = div()
            .id("dotdir")
            .key_context("DotDir")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(p.bg)
            .text_color(p.fg)
            .text_size(px(self.settings.font_size))
            .font_family(".AppleSystemUIFont")
            .on_action(cx.listener(Self::on_command))
            .on_modifiers_changed(
                cx.listener(|this, event: &ModifiersChangedEvent, window, cx| {
                    if !event.shift {
                        this.end_shift_selection();
                    }
                    cx.notify();
                    window.refresh();
                }),
            )
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.handle_panel_menu_key(event, window, cx) {
                    cx.stop_propagation();
                    window.prevent_default();
                    return;
                }
                if event.keystroke.key == "escape" {
                    if this.dismiss_hover(cx) {
                        cx.stop_propagation();
                        window.prevent_default();
                        return;
                    }
                } else if !this.hover.contains(window.mouse_position()) {
                    this.dismiss_hover(cx);
                }
                if this.modal.is_some() {
                    let key = event.keystroke.key.as_str();
                    let transfer = matches!(
                        this.modal,
                        Some(Modal::Prompt {
                            kind: PromptKind::Transfer { .. },
                            ..
                        })
                    );
                    if transfer && this.transfer_dropdown_open.is_some() {
                        return;
                    }
                    if transfer && key == "f10" && !event.keystroke.modifiers.modified() {
                        this.choose_transfer_destination(window, cx);
                        cx.stop_propagation();
                        window.prevent_default();
                    } else if key == "escape" {
                        this.dismiss(window, cx);
                        cx.stop_propagation();
                        window.prevent_default();
                    } else if matches!(this.modal, Some(Modal::Conflict { .. }))
                        && matches!(key, "left" | "right" | "tab")
                    {
                        this.palette_cursor =
                            if key == "left" || (key == "tab" && event.keystroke.modifiers.shift) {
                                (this.palette_cursor + 4) % 5
                            } else {
                                (this.palette_cursor + 1) % 5
                            };
                        cx.notify();
                        window.refresh();
                        cx.stop_propagation();
                        window.prevent_default();
                    } else if matches!(this.modal, Some(Modal::Palette | Modal::QuickPick { .. }))
                        && matches!(key, "up" | "down")
                    {
                        let len = match &this.modal {
                            Some(Modal::QuickPick { request, .. }) => {
                                request.matches(&this.dialog_input.read(cx).value()).len()
                            }
                            _ => this.filtered_commands(cx).len(),
                        };
                        this.palette_cursor = if key == "up" {
                            this.palette_cursor.saturating_sub(1)
                        } else {
                            (this.palette_cursor + 1).min(len.saturating_sub(1))
                        };
                        cx.notify();
                        cx.stop_propagation();
                        window.prevent_default();
                    } else if matches!(this.modal, Some(Modal::OpenVsx))
                        && matches!(key, "up" | "down")
                        && this
                            .dialog_input
                            .read(cx)
                            .focus_handle(cx)
                            .is_focused(window)
                    {
                        this.move_market_selection(key == "down", cx);
                        cx.stop_propagation();
                        window.prevent_default();
                    } else if matches!(this.modal, Some(Modal::Search))
                        && matches!(key, "up" | "down")
                    {
                        if let Some(s) = &mut this.search {
                            s.cursor = if key == "up" {
                                s.cursor.saturating_sub(1)
                            } else {
                                (s.cursor + 1).min(s.results.len().saturating_sub(1))
                            };
                        }
                        cx.notify();
                        cx.stop_propagation();
                        window.prevent_default();
                    } else if matches!(this.modal, Some(Modal::Search))
                        && key == "enter"
                        && event.keystroke.modifiers.platform
                    {
                        if let Some(s) = &this.search {
                            this.open_search_result(s.cursor, window, cx);
                        }
                        cx.stop_propagation();
                        window.prevent_default();
                    } else if key == "enter"
                        && !event.keystroke.modifiers.shift
                        && (!transfer
                            || this
                                .dialog_input
                                .read(cx)
                                .focus_handle(cx)
                                .is_focused(window)
                            || this.copy_use_filter
                                && this
                                    .copy_filter
                                    .read(cx)
                                    .focus_handle(cx)
                                    .is_focused(window))
                        && (!matches!(this.modal, Some(Modal::OpenVsx))
                            || this
                                .dialog_input
                                .read(cx)
                                .focus_handle(cx)
                                .is_focused(window))
                    {
                        this.accept_modal(window, cx);
                        cx.stop_propagation();
                        window.prevent_default();
                    }
                }
            }));
        if self.panels_visible {
            let panes = div()
                .flex()
                .flex_1()
                .min_h_0()
                .gap(px(4.))
                .child(self.render_pane(0, window, cx))
                .child(self.render_pane(1, window, cx));
            root = root.child(panes);
        }
        if self.terminal_visible {
            root = root.child(self.render_terminal(window, cx));
        }
        if let Some(op) = &self.operation {
            root = root.child(
                div()
                    .id(("operation-progress", op.id as usize))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .h(px(30.))
                    .flex_shrink_0()
                    .bg(p.input)
                    .child(
                        text(format!(
                            "{} · {:.0}% · {}",
                            op.kind.title(),
                            op.fraction * 100.,
                            op.progress
                        ))
                        .flex_1()
                        .text_size(px(12.)),
                    )
                    .child(self.button(
                        "cancel-operation",
                        if op.token.is_cancelled() {
                            "Cancelling…"
                        } else {
                            "Cancel"
                        },
                        |this, _, cx| {
                            if let Some(op) = &this.operation {
                                op.token.cancel();
                            }
                            cx.notify();
                        },
                        cx,
                    )),
            );
        }
        let location = self.pane_location(self.active);
        let path = if matches!(location, Location::Provider(_)) {
            location.display()
        } else {
            crate::archive::display_local_path(&self.cwd(self.active))
        };
        root = root.child(
            div()
                .h(px(26.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(6.))
                .bg(p.command_bg)
                .font_family("Menlo")
                .text_size(px(12.))
                .child(text(format!("{path}> ")).font_weight(FontWeight::SEMIBOLD))
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&self.command_line)
                            .appearance(false)
                            .bordered(false)
                            .h(px(24.)),
                    ),
                ),
        );
        let mut terminal_tabs = div()
            .h(px(30.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(8.))
            .border_t_1()
            .border_color(p.border);
        for (i, t) in self.terminals.iter().enumerate() {
            let label = format!(
                "{} {}",
                if t.closed.load(std::sync::atomic::Ordering::Relaxed) {
                    "○"
                } else {
                    "●"
                },
                t.title.lock().unwrap()
            );
            terminal_tabs = terminal_tabs.child(
                self.button(
                    ("terminal-tab", t.id as usize),
                    label,
                    move |this, window, cx| {
                        this.terminal_active = i;
                        this.terminal_visible = true;
                        this.terminal_focus.focus(window, cx);
                        cx.notify();
                    },
                    cx,
                )
                .aria_description(t.shell.clone())
                .when(i == self.terminal_active, |s| s.border_color(p.accent)),
            );
            terminal_tabs = terminal_tabs.child(self.button(
                ("terminal-close", i),
                "×",
                move |this, _, cx| {
                    this.terminals.remove(i);
                    this.terminal_active = this
                        .terminal_active
                        .min(this.terminals.len().saturating_sub(1));
                    cx.notify();
                },
                cx,
            ));
        }
        if self.terminals.is_empty() {
            terminal_tabs = terminal_tabs.child(
                self.button(
                    "start-shell",
                    "●  zsh",
                    |this, window, cx| {
                        this.ensure_terminal(cx);
                        this.terminal_visible = true;
                        this.terminal_focus.focus(window, cx);
                        cx.notify();
                    },
                    cx,
                )
                .bg(p.accent)
                .text_color(p.accent_fg),
            );
        }
        terminal_tabs = terminal_tabs
            .child(
                text(self.status.clone())
                    .flex_1()
                    .text_size(px(11.))
                    .text_color(p.muted),
            )
            .child(
                self.button(
                    "new-terminal",
                    "+",
                    |this, window, cx| {
                        this.new_terminal(cx);
                        this.terminal_visible = true;
                        this.terminal_focus.focus(window, cx);
                        cx.notify();
                    },
                    cx,
                )
                .bg(p.accent)
                .text_color(p.accent_fg),
            )
            .child(text("SHELL").text_size(px(11.)).text_color(p.muted))
            .child(
                text(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()))
                    .text_size(px(12.))
                    .font_family("Menlo"),
            );
        root = root.child(terminal_tabs);
        let mut keybar = div().flex().h(px(24.)).flex_shrink_0().gap(px(1.));
        let bindings = {
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            // GPUI's dispatch tree is empty during the first render. Wait for
            // the focused element to be mounted before inspecting that tree.
            let contexts = if window
                .focused(cx)
                .is_some_and(|focus| focus.is_rendered(window))
            {
                window.context_stack()
            } else {
                let view = cx.weak_entity();
                window.on_next_frame(move |_, cx| {
                    let _ = view.update(cx, |_, cx| cx.notify());
                });
                Vec::new()
            };
            (1..=12)
                .map(|number| function_key_command(&keymap, &contexts, number, window.modifiers()))
                .collect::<Vec<_>>()
        };
        let command_state = self.command_state(cx);
        for (i, id) in bindings.into_iter().enumerate() {
            let label = id
                .as_deref()
                .and_then(|id| {
                    COMMANDS
                        .iter()
                        .find(|command| command.id == id)
                        .map(|command| {
                            command
                                .keybar_label(
                                    self.active_document().is_some() || self.active_hex().is_some(),
                                    &command_state,
                                )
                                .to_string()
                        })
                        .or_else(|| {
                            self.extensions
                                .iter()
                                .flat_map(|extension| &extension.contributions)
                                .find(|command| command.id == id && command.viewer.is_none())
                                .map(|command| command.title.clone())
                        })
                })
                .unwrap_or_default();
            let enabled = self.modal.is_none()
                && id
                    .as_deref()
                    .is_some_and(|id| self.command_available(id, cx));
            keybar = keybar.child(
                div()
                    .id(("keybar", i))
                    .role(Role::Button)
                    .aria_label(format!("F{} {label}", i + 1))
                    .aria_description(if enabled {
                        ""
                    } else if id.is_some() {
                        "Unavailable in the current context"
                    } else {
                        "No command bound"
                    })
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .justify_center()
                    .gap(px(5.))
                    .px(px(5.))
                    .cursor_default()
                    .when(enabled, |s| {
                        s.cursor_pointer().hover(move |s| s.bg(p.selected))
                    })
                    .when(!enabled, |s| s.opacity(0.4))
                    .child(
                        div()
                            .rounded(px(2.))
                            .px(px(2.))
                            .bg(p.accent)
                            .text_color(p.accent_fg)
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("F{}", i + 1)),
                    )
                    .child(text(label).text_size(px(12.)).text_color(p.muted))
                    // Keep the target's keyboard context while clicking the bar.
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if enabled && let Some(id) = &id {
                            this.run(id, window, cx);
                        }
                    })),
            );
        }
        root = root.child(keybar);
        if self.modal.is_some() {
            root = root.child(self.render_modal(window, cx));
        }
        root = root.children(self.render_hover(window, cx));
        root
    }
}
