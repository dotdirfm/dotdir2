use crate::{
    app::*,
    commands::{COMMANDS, function_key_command},
    model::*,
    terminal,
};
use dotdir_core::copy::{ConflictPolicy, ConflictResolution};
use dotdir_extension_host::PanelNode;
use gpui_kit::{
    component::{
        Icon, IconName,
        input::{Editor, Input},
    },
    prelude::FluentBuilder,
    *,
};
use serde_json::json;

fn icon(name: IconName, color: Hsla) -> impl IntoElement {
    Icon::new(name).size(px(16.)).text_color(color)
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
        icon(IconName::File, color).into_any_element()
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
    pub(crate) fn entry_icon_sized(&self, entry: &Entry, size: f32) -> AnyElement {
        if !entry.parent
            && let Some(path) = self.icon_theme.as_ref().and_then(|theme| {
                theme.icon(&entry.name, entry.directory, self.settings.theme == "light")
            })
        {
            let source = if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
            {
                crate::icon_theme::sized_icon(path, size)
            } else {
                ImageSource::from(path.to_path_buf())
            };
            return img(source)
                .size(px(size))
                .flex_shrink_0()
                .object_fit(ObjectFit::Contain)
                .into_any_element();
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
            .border_color(p.border)
            .bg(p.input)
            .cursor_pointer()
            .hover(move |s| s.bg(p.selected))
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                f(this, window, cx);
                if !this.shutting_down {
                    window.refresh();
                }
            }))
    }
    fn render_pane(
        &mut self,
        pane: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let active = self.active == pane;
        let mut tabs = div()
            .flex()
            .h(px(28.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(p.border)
            .overflow_hidden();
        for (index, tab) in self.panes[pane].tabs.iter().enumerate() {
            let id = tab.id();
            let preview = tab.preview;
            let selected = self.panes[pane].active == index;
            let label = match &tab.content {
                TabContent::Directory(d) => d
                    .current()
                    .path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".into()),
                TabContent::Document(id) => self
                    .documents
                    .get(id)
                    .map(|d| {
                        format!(
                            "{}{}",
                            if d.dirty(cx) { "● " } else { "" },
                            d.path.file_name().unwrap_or_default().to_string_lossy()
                        )
                    })
                    .unwrap_or_default(),
                TabContent::Image { path, .. } | TabContent::Media { path, .. } => path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                TabContent::Panel { title, .. } => title.clone(),
                TabContent::Pending { path, .. } => path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Preview".into()),
            };
            tabs = tabs.child(
                div()
                    .id(("tab", (pane * 10000 + index)))
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
                    .min_w(px(48.))
                    .max_w(px(200.))
                    .px(px(8.))
                    .border_r_1()
                    .border_color(p.border)
                    .when(selected, |s| {
                        s.border_b_2()
                            .border_color(if active { p.accent } else { p.muted })
                    })
                    .text_color(if selected { p.fg } else { p.muted })
                    .text_size(px(12.))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.set_tab(pane, index, window, cx);
                        if preview && event.click_count() == 2 {
                            this.run("keep-preview", window, cx);
                        }
                    }))
                    .child(text(label).when(tab.preview, |text| text.italic()))
                    .child(
                        div()
                            .id(("close-tab", pane * 10000 + index))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(16.))
                            .rounded(px(2.))
                            .hover(move |s| s.bg(p.selected))
                            .child(icon(IconName::Close, p.muted))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.request_close_tab(pane, id, window, cx);
                            })),
                    ),
            )
        }
        tabs = tabs.child(div().flex_1()).child(
            div()
                .id(("pane-menu", pane))
                .w(px(28.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(p.accent)
                .text_color(p.accent_fg)
                .cursor_pointer()
                .child(icon(IconName::EllipsisVertical, p.accent_fg))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.active = pane;
                    this.show_modal(Modal::Palette, window, cx);
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
                let doc = &self.documents[id];
                let path = doc.path.display().to_string();
                let state = doc.input.clone();
                let readonly = doc.readonly;
                let status = doc.error.clone().unwrap_or_else(|| {
                    if doc.saving {
                        "Saving…".into()
                    } else if readonly {
                        "Read only".into()
                    } else if doc.dirty(cx) {
                        "Modified · F2 / ⌘S to save".into()
                    } else {
                        "Saved".into()
                    }
                });
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .h(px(34.))
                            .flex_shrink_0()
                            .px(px(12.))
                            .py(px(8.))
                            .border_b_1()
                            .border_color(p.border)
                            .text_size(px(12.))
                            .font_family("Menlo")
                            .child(text(path)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .font_family("Menlo")
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
            TabContent::Media { view, .. } => view.clone().into_any_element(),
            TabContent::Image { path, .. } => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .when(active, |s| s.track_focus(&self.focus))
                .key_context("PreviewPane")
                .child(
                    div()
                        .h(px(34.))
                        .px(px(12.))
                        .py(px(8.))
                        .text_size(px(12.))
                        .child(text(path.display().to_string())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .child(img(path.clone()).size_full().object_fit(ObjectFit::Contain)),
                )
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
            .child(tabs)
            .child(content)
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
                let mut table = div().flex().flex_col().border_1().border_color(p.border);
                let header = div().flex().bg(p.input).children(
                    columns
                        .into_iter()
                        .map(|c| text(c).flex_1().p(px(8.)).font_weight(FontWeight::SEMIBOLD)),
                );
                table = table.child(header);
                for (i, row) in rows.into_iter().enumerate() {
                    table = table.child(
                        div()
                            .flex()
                            .when(i % 2 == 0, |s| s.bg(p.selected))
                            .children(row.into_iter().map(|c| text(c).flex_1().p(px(8.)))),
                    );
                }
                table.into_any_element()
            }
        }
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

    fn render_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.palette;
        let Some(modal) = &self.modal else {
            return div().into_any_element();
        };
        let (title, description) = match modal {
            Modal::Prompt {
                title, description, ..
            } => (title.clone(), description.clone()),
            Modal::Palette => ("Commands".into(), String::new()),
            Modal::Settings => ("Settings".into(), String::new()),
            Modal::Extensions => (
                "Extensions".into(),
                "Installed JavaScript extensions".into(),
            ),
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
            .w(px(620.))
            .max_w(relative(0.9))
            .max_h(relative(0.82))
            .overflow_y_scroll()
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
                            .when(selected, |s| s.bg(p.selected))
                            .cursor_pointer()
                            .hover(move |s| s.bg(p.selected))
                            .child(text(title).flex_1())
                            .child(
                                text(key.replace("cmd-", "⌘"))
                                    .text_color(p.accent)
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
            Modal::Prompt { kind, .. } => {
                if !matches!(
                    kind,
                    PromptKind::Trash(_) | PromptKind::CloseTab { .. } | PromptKind::Quit
                ) {
                    content = content.child(Input::new(&self.dialog_input));
                }
                if matches!(kind, PromptKind::Transfer(..)) {
                    let policies = [
                        ("Ask", ConflictPolicy::Ask),
                        ("Overwrite", ConflictPolicy::Overwrite),
                        ("Skip", ConflictPolicy::Skip),
                        ("Rename", ConflictPolicy::Rename),
                        ("Append", ConflictPolicy::Append),
                        ("Newer", ConflictPolicy::OnlyNewer),
                    ];
                    content = content.child(
                        text("If the destination exists")
                            .text_color(p.muted)
                            .text_size(px(12.)),
                    );
                    let mut choices = div().flex().gap(px(4.));
                    for (i, (label, policy)) in policies.into_iter().enumerate() {
                        let selected = std::mem::discriminant(&policy)
                            == std::mem::discriminant(&self.copy_options.conflict_policy);
                        choices = choices.child(self.button(
                            ("policy", i),
                            format!("{}{}", if selected { "✓ " } else { "" }, label),
                            move |this, _, cx| {
                                this.copy_options.conflict_policy = policy.clone();
                                cx.notify();
                            },
                            cx,
                        ));
                    }
                    content = content.child(choices);
                    let mut options = div().flex().gap(px(8.));
                    for (id, label, on) in [
                        (0, "Permissions", self.copy_options.copy_permissions),
                        (1, "Extended attributes", self.copy_options.copy_xattrs),
                        (2, "Copy on write", self.copy_options.use_cow),
                    ] {
                        options = options.child(self.button(
                            ("copy-option", id as usize),
                            format!("{} {label}", if on { "☑" } else { "☐" }),
                            move |this, _, cx| {
                                match id {
                                    0 => {
                                        this.copy_options.copy_permissions =
                                            !this.copy_options.copy_permissions
                                    }
                                    1 => {
                                        this.copy_options.copy_xattrs =
                                            !this.copy_options.copy_xattrs
                                    }
                                    _ => this.copy_options.use_cow = !this.copy_options.use_cow,
                                }
                                cx.notify();
                            },
                            cx,
                        ));
                    }
                    content = content.child(options);
                }
                let confirm = match kind {
                    PromptKind::Trash(_) => "Move to Trash",
                    PromptKind::CloseTab { .. } => "Discard Changes",
                    PromptKind::Quit => "Close Without Saving",
                    PromptKind::Transfer(kind, _) => kind.title(),
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
                            if self.settings.theme == id {
                                "✓ "
                            } else {
                                ""
                            },
                            label
                        ),
                        move |this, _, cx| {
                            this.settings.theme = id.into();
                            this.palette = crate::theme::Palette::named(id);
                            this.palette.apply(id, cx);
                            this.persist();
                            cx.notify();
                        },
                        cx,
                    ));
                }
                content = content.child(themes);
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
                content=content.child(self.button("keybindings","Edit keybindings.json",|this,window,cx|{let path=this.settings_path.with_file_name("keybindings.json");if !path.exists(){let _=std::fs::write(&path,"[\n  {\"key\": \"cmd-l\", \"command\": \"location\", \"context\": \"DotDir\"}\n]\n");}this.dismiss(window,cx);this.open_document(this.active,path,false,window,cx);},cx));
            }
            Modal::Extensions => {
                content = content.child(self.icon_theme_controls("extension-icons", cx));
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
                for error in &self.extension_errors {
                    content = content.child(text(error.clone()).text_color(p.error));
                }
                content = content.child(self.button(
                    "reload-extensions",
                    "Reload Extensions",
                    |this, _, cx| {
                        this.extension_errors.clear();
                        this.load_extensions();
                        cx.notify();
                    },
                    cx,
                ));
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
            .on_modifiers_changed(cx.listener(|_, _, window, cx| {
                cx.notify();
                window.refresh();
            }))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.modal.is_some() {
                    let key = event.keystroke.key.as_str();
                    if key == "escape" {
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
                    } else if matches!(this.modal, Some(Modal::Palette))
                        && matches!(key, "up" | "down")
                    {
                        let len = this.filtered_commands(cx).len();
                        this.palette_cursor = if key == "up" {
                            this.palette_cursor.saturating_sub(1)
                        } else {
                            (this.palette_cursor + 1).min(len.saturating_sub(1))
                        };
                        cx.notify();
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
                    } else if key == "enter" && !event.keystroke.modifiers.shift {
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
        let path = self.cwd(self.active).display().to_string();
        root = root.child(
            div()
                .h(px(26.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(6.))
                .bg(rgb(0x171b24))
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
        for (i, id) in bindings.into_iter().enumerate() {
            let label = id
                .as_deref()
                .and_then(|id| {
                    COMMANDS
                        .iter()
                        .find(|command| command.id == id)
                        .map(|command| {
                            command
                                .keybar_label(self.active_document().is_some())
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
        root
    }
}
