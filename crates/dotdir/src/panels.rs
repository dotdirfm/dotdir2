//! Directory presentations share selection, commands, watches and icon assets.
use crate::{app::*, column_scroll::COLUMN_WIDTH, model::*, ui::text};
use gpui_kit::{component::scroll::ScrollableElement, prelude::FluentBuilder, *};

fn modified(entry: &Entry) -> String {
    if entry.parent {
        return String::new();
    }
    chrono::DateTime::from_timestamp_millis(entry.modified as i64)
        .map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%d.%m.%Y %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}
fn kind(entry: &Entry) -> String {
    if entry.parent {
        "Parent".into()
    } else if entry.symlink {
        "Link".into()
    } else if entry.directory {
        "Folder".into()
    } else {
        entry
            .path
            .extension()
            .map(|s| s.to_string_lossy().to_uppercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "File".into())
    }
}
impl DotDir {
    pub(crate) fn panel_view_selector(&self, pane: usize, cx: &Context<Self>) -> Div {
        let p = self.palette;
        let selected = self.settings.presentation(pane);
        let mut selector = div().flex().items_center().gap(px(3.));
        for (index, mode) in Presentation::ALL.into_iter().enumerate() {
            selector = selector.child(
                div()
                    .id(("view-mode", pane * 10 + index))
                    .role(Role::Button)
                    .aria_label(format!("View: {}", mode.label()))
                    .aria_selected(selected == mode)
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(3.))
                    .text_size(px(11.))
                    .cursor_pointer()
                    .bg(if selected == mode { p.selected } else { p.bg })
                    .text_color(if selected == mode { p.accent } else { p.muted })
                    .hover(move |s| s.bg(p.selected))
                    .child(mode.label())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_presentation(pane, mode, window, cx)
                    })),
            );
        }
        selector
    }
    fn panel_item(
        &self,
        location: (usize, usize),
        index: usize,
        directory: &Directory,
        presentation: Presentation,
        active: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let (pane, column) = location;
        let p = self.palette;
        let entry = &directory.entries[index];
        let cursor = directory.cursor == index;
        let marked = directory.marked.contains(&entry.path);
        let icons = presentation == Presentation::Icons;
        let mut item = div()
            .id(SharedString::from(format!(
                "file-{pane}-{}-{index}",
                directory.id
            )))
            .role(Role::ListBoxOption)
            .aria_label(entry.name.clone())
            .aria_description(format!(
                "{}; {} bytes; {}; mode {:o}",
                kind(entry),
                entry.size,
                modified(entry),
                entry.mode & 0o7777
            ))
            .aria_selected(marked || cursor)
            .when(cursor && active, |s| s.aria_active_descendant())
            .relative()
            .flex()
            .flex_shrink_0()
            .overflow_hidden()
            .cursor_default()
            .when(marked, |s| s.bg(p.marked))
            .when(cursor, |s| s.bg(p.selected))
            .when(cursor && active, |s| {
                s.child(
                    div()
                        .absolute()
                        .inset_0()
                        .border_1()
                        .border_color(p.accent)
                        .when(icons, |s| s.rounded(px(5.))),
                )
            })
            .when(!cursor && !marked, |s| s.hover(move |s| s.bg(p.selected)));
        let name = format!("{}{}", entry.name, if entry.symlink { " ↗" } else { "" });
        if icons {
            item = item
                .flex_col()
                .items_center()
                .justify_center()
                .h(px(112.))
                .flex_1()
                .min_w_0()
                .mx(px(3.))
                .my(px(2.))
                .rounded(px(5.))
                .gap(px(5.))
                .px(px(4.))
                .child(self.entry_icon_sized(entry, 56.))
                .child(
                    div()
                        .w_full()
                        .h(px(32.))
                        .text_center()
                        .text_size(px(12.))
                        .line_clamp(2)
                        .child(name),
                );
        } else {
            item = item
                .items_center()
                .h(px(26.))
                .px(px(10.))
                .gap(px(8.))
                .child(self.entry_icon(entry))
                .child(
                    text(name)
                        .flex_1()
                        .min_w_0()
                        .when(entry.directory, |s| s.font_weight(FontWeight::SEMIBOLD))
                        .when(entry.hidden && !entry.directory, |s| s.text_color(p.muted)),
                );
            if presentation == Presentation::Details {
                item = item
                    .child(
                        text(if entry.directory {
                            "—".into()
                        } else {
                            human_size(entry.size)
                        })
                        .w(px(64.))
                        .text_right()
                        .text_color(p.muted)
                        .text_size(px(11.)),
                    )
                    .child(
                        text(kind(entry))
                            .w(px(60.))
                            .text_color(p.muted)
                            .text_size(px(11.)),
                    )
                    .child(
                        text(modified(entry))
                            .w(px(126.))
                            .text_color(p.muted)
                            .text_size(px(11.)),
                    );
            } else if presentation == Presentation::Columns {
                if entry.directory {
                    item =
                        item.child(text(if entry.parent { "↑" } else { "›" }).text_color(p.muted));
                }
            } else if !entry.directory {
                item = item.child(
                    text(human_size(entry.size))
                        .text_size(px(11.))
                        .text_color(p.muted),
                );
            }
        }
        item.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.select_panel_entry(pane, column, index, event, window, cx);
            }),
        )
        .into_any_element()
    }
    fn panel_entries(
        &mut self,
        pane: usize,
        column: usize,
        mut directory: Directory,
        layout: PanelLayout,
        active: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        layout.ensure_visible(&mut directory);
        if let Some(actual) = self.directory_by_id(directory.id) {
            actual.top = directory.top;
        }
        let mut content = div().flex().flex_1().min_h_0().overflow_hidden();
        if layout.presentation == Presentation::Icons {
            content = content.flex_col();
            for row in 0..layout.rows {
                let mut line = div().flex().h(px(layout.cell_height)).flex_shrink_0();
                for col in 0..layout.columns {
                    let index = directory.top + row * layout.columns + col;
                    if index < directory.entries.len() {
                        line = line.child(self.panel_item(
                            (pane, column),
                            index,
                            &directory,
                            layout.presentation,
                            active,
                            cx,
                        ));
                    } else {
                        line = line.child(div().flex_1().min_w_0());
                    }
                }
                content = content.child(line);
            }
        } else {
            for col in 0..layout.columns {
                let mut line = div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .when(col + 1 < layout.columns, |s| {
                        s.border_r_1().border_color(p.border)
                    });
                for row in 0..layout.rows {
                    let index = directory.top + col * layout.rows + row;
                    if index < directory.entries.len() {
                        line = line.child(self.panel_item(
                            (pane, column),
                            index,
                            &directory,
                            layout.presentation,
                            active,
                            cx,
                        ));
                    }
                }
                content = content.child(line);
            }
        }
        if let Some(error) = directory.error.clone() {
            content = content.child(div().p(px(10.)).text_color(p.error).child(error));
        }
        let id = directory.id;
        div()
            .id(SharedString::from(format!("file-list-{pane}-{id}")))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if let Some(browser) = this.browser_mut(pane) {
                        browser.active = column;
                    }
                    this.focus_pane(pane, window, cx);
                }),
            )
            .on_scroll(cx.listener(move |this, event: &ScrollEvent, window, cx| {
                let delta = event.delta.pixel_delta(px(layout.cell_height));
                let (x, y): (f32, f32) = (delta.x.into(), delta.y.into());
                if event.modifiers.shift || x.abs() > y.abs() || y == 0. {
                    return;
                }
                if layout.presentation == Presentation::Columns
                    && let Some(browser) = this.browser(pane)
                {
                    let browser_id = browser.id;
                    this.panel_scrolls
                        .entry(browser_id)
                        .or_default()
                        .hold_position();
                }
                let changed = this
                    .directory_by_id(id)
                    .is_some_and(|directory| layout.scroll_pixels(directory, -y));
                cx.stop_propagation();
                if changed {
                    if let Some(browser) = this.browser_mut(pane) {
                        browser.active = column;
                    }
                    this.update_column_preview(pane);
                    cx.notify();
                    window.refresh();
                }
            }))
            .child(content)
            .into_any_element()
    }
    pub(crate) fn render_directory(
        &mut self,
        pane: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let active = self.active == pane;
        let browser = self.browser(pane).expect("directory tab").clone();
        let d = browser.current();
        let layout = self.layout_for(pane, window);
        let mut breadcrumbs = div()
            .flex()
            .items_center()
            .gap(px(5.))
            .h(px(34.))
            .flex_shrink_0()
            .px(px(12.))
            .border_b_1()
            .border_color(p.border)
            .text_size(px(12.))
            .overflow_hidden();
        for (index, path) in d
            .path
            .ancestors()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .enumerate()
        {
            let path = path.to_path_buf();
            let label = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".into());
            if index > 1 {
                breadcrumbs = breadcrumbs.child(text("/").text_color(p.muted));
            }
            breadcrumbs = breadcrumbs.child(
                div()
                    .id(SharedString::from(format!(
                        "crumb-{pane}-{}",
                        path.display()
                    )))
                    .role(Role::Button)
                    .aria_label(label.clone())
                    .cursor_pointer()
                    .hover(move |s| s.text_color(p.accent))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.navigate(pane, path.clone(), window, cx)
                    })),
            );
        }
        let mut toolbar = div()
            .flex()
            .items_center()
            .h(px(30.))
            .flex_shrink_0()
            .px(px(6.))
            .gap(px(4.))
            .border_b_1()
            .border_color(p.border)
            .child(self.panel_view_selector(pane, cx));
        let contents = if layout.presentation == Presentation::Columns {
            let motion = self.panel_scrolls.entry(browser.id).or_default();
            let column_count = browser.columns.len();
            let animating = motion.advance(column_count, std::time::Instant::now());
            if cx.reduce_motion() {
                motion.finish();
            }
            if animating && !cx.reduce_motion() {
                window.request_animation_frame();
            }
            let trailing_space = motion.trailing_space(column_count);
            let scroll = motion.handle.clone();
            let browser_id = browser.id;
            toolbar = toolbar.child(div().flex_1());
            for (direction, label) in [(-1, "‹"), (1, "›")] {
                toolbar = toolbar.child(
                    div()
                        .id(("scroll-columns", pane * 2 + usize::from(direction > 0)))
                        .role(Role::Button)
                        .aria_label(if direction < 0 {
                            "Scroll columns left"
                        } else {
                            "Scroll columns right"
                        })
                        .px(px(8.))
                        .text_color(p.muted)
                        .cursor_pointer()
                        .child(label)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(motion) = this.panel_scrolls.get_mut(&browser_id) {
                                motion.step(direction, column_count);
                            }
                            window.refresh();
                            cx.notify();
                        })),
                );
            }
            let mut columns = div()
                .id(("finder-columns", browser.id))
                .flex()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .w_full()
                // The scroll handler below owns the horizontal input.
                .overflow_hidden()
                .track_scroll(&scroll)
                .on_scroll(cx.listener(move |this, event: &ScrollEvent, window, cx| {
                    let delta = event.delta.pixel_delta(px(26.));
                    let (x, y): (f32, f32) = (delta.x.into(), delta.y.into());
                    let amount = if event.modifiers.shift {
                        if x == 0. { y } else { x }
                    } else if x.abs() > y.abs() {
                        x
                    } else {
                        return;
                    };
                    if amount == 0. {
                        return;
                    }
                    let Some(motion) = this.panel_scrolls.get_mut(&browser_id) else {
                        return;
                    };
                    motion.scroll_by(amount, column_count);
                    cx.stop_propagation();
                    window.refresh();
                    cx.notify();
                }));
            for (index, directory) in browser.columns.iter().enumerate() {
                let title = directory
                    .path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".into());
                columns = columns.child(
                    div()
                        .w(px(COLUMN_WIDTH))
                        .min_w(px(COLUMN_WIDTH))
                        .h_full()
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .border_r_1()
                        .border_color(p.border)
                        .child(
                            div()
                                .id(("column-header", directory.id))
                                .role(Role::Button)
                                .aria_label(format!("Column: {title}"))
                                .h(px(26.))
                                .flex_shrink_0()
                                .px(px(10.))
                                .py(px(4.))
                                .text_size(px(11.))
                                .bg(p.secondary)
                                .text_color(if active && browser.active == index {
                                    p.accent
                                } else {
                                    p.muted
                                })
                                .child(text(title))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if let Some(browser) = this.browser_mut(pane) {
                                        browser.active = index;
                                    }
                                    this.focus_pane(pane, window, cx);
                                    this.persist();
                                })),
                        )
                        .child(self.panel_entries(
                            pane,
                            index,
                            directory.clone(),
                            layout,
                            active && browser.active == index,
                            cx,
                        )),
                );
            }
            if trailing_space > 0. {
                columns = columns.child(div().w(px(trailing_space)).flex_shrink_0());
            }
            // Keep the overlay outside the scrolling strip: an absolute child
            // of that strip inherits its content extent and scroll offset.
            div()
                .id(("columns-viewport", browser.id))
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .w_full()
                .overflow_hidden()
                .child(columns)
                .horizontal_scrollbar(&scroll)
                .into_any_element()
        } else {
            let mut list = div().flex().flex_col().flex_1().min_h_0();
            if layout.presentation == Presentation::Details {
                let mut header = div()
                    .flex()
                    .items_center()
                    .h(px(26.))
                    .flex_shrink_0()
                    .px(px(10.))
                    .gap(px(8.))
                    .text_size(px(11.))
                    .text_color(p.muted)
                    .bg(p.secondary);
                for (label, command, width) in [
                    ("Name", "sort-name", 0.),
                    ("Size", "sort-size", 64.),
                    ("Kind", "sort-extension", 60.),
                    ("Modified", "sort-date", 126.),
                ] {
                    header = header.child(
                        div()
                            .id(SharedString::from(format!("sort-{pane}-{command}")))
                            .role(Role::Button)
                            .aria_label(format!("Sort by {label}"))
                            .when(width == 0., |s| s.flex_1().min_w_0())
                            .when(width > 0., |s| s.w(px(width)))
                            .when(label == "Size", |s| s.text_right())
                            .cursor_pointer()
                            .hover(move |s| s.text_color(p.accent))
                            .child(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.active = pane;
                                this.run(command, window, cx);
                            })),
                    );
                }
                list = list.child(header);
            }
            list.child(self.panel_entries(pane, browser.active, d.clone(), layout, active, cx))
                .into_any_element()
        };
        let count = d
            .entries
            .iter()
            .filter(|e| !e.parent && !e.directory)
            .count();
        let bytes = d
            .entries
            .iter()
            .filter(|e| !e.parent && !e.directory)
            .map(|e| e.size)
            .sum();
        let summary = if d.loading {
            "Loading…".into()
        } else if !d.marked.is_empty() {
            format!(
                "{} marked · {}",
                d.marked.len(),
                human_size(d.selected().iter().map(|e| e.size).sum())
            )
        } else {
            format!("{count} files, {}", human_size(bytes))
        };
        let focused = d.focused();
        let status = focused
            .map(|e| {
                if e.parent {
                    "Up".into()
                } else if e.directory {
                    "<DIR>".into()
                } else {
                    human_size(e.size)
                }
            })
            .unwrap_or_default();
        let focus = self.focus.clone();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(breadcrumbs)
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .key_context("FilePane")
                    .when(active, |s| s.track_focus(&focus))
                    .on_key_down(cx.listener(Self::pane_key))
                    .child(contents),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(26.))
                    .flex_shrink_0()
                    .px(px(12.))
                    .border_t_1()
                    .border_color(p.border)
                    .text_size(px(11.))
                    .text_color(p.muted)
                    .child(text(focused.map(|e| e.name.clone()).unwrap_or_default()).flex_1())
                    .child(text(status))
                    .child(text(focused.map(modified).unwrap_or_default())),
            )
            .child(
                div()
                    .h(px(18.))
                    .flex_shrink_0()
                    .px(px(12.))
                    .border_t_1()
                    .border_color(p.border)
                    .text_size(px(11.))
                    .text_color(p.muted)
                    .child(summary),
            )
            .into_any_element()
    }
}
