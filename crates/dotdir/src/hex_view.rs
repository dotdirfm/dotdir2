use crate::{
    archive::SourceStamp,
    hex_file::{self, ByteEdit, ByteSource, HexFile, PAGE_BYTES},
    theme::Palette,
};
use gpui_kit::{
    component::input::{Input, InputEvent, InputState},
    component::slider::{Slider, SliderEvent, SliderState},
    prelude::FluentBuilder,
    *,
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

const ROW_BYTES: u64 = 16;
const ROW_HEIGHT: f32 = 20.;
const MAX_ROWS: u64 = 256;
const SCROLL_RANGE: f32 = 1_000_000.;

pub struct HexBuffer {
    source: ByteSource,
    stamp: Option<Arc<SourceStamp>>,
    edits: Arc<BTreeMap<u64, ByteEdit>>,
    revision: u64,
    pub saving: bool,
    error: Option<String>,
}

impl HexBuffer {
    pub fn new(file: HexFile) -> Self {
        Self {
            source: file.source,
            stamp: file.stamp.map(Arc::new),
            edits: Arc::default(),
            revision: 0,
            saving: false,
            error: None,
        }
    }

    pub fn dirty(&self) -> bool {
        !self.edits.is_empty()
    }

    pub fn discard(&mut self, cx: &mut Context<Self>) {
        self.edits = Arc::default();
        self.error = None;
        cx.notify();
    }

    fn replace_byte(&mut self, offset: u64, original: u8, value: u8, cx: &mut Context<Self>) {
        let edits = Arc::make_mut(&mut self.edits);
        if original == value {
            edits.remove(&offset);
        } else {
            edits.insert(
                offset,
                ByteEdit {
                    before: original,
                    after: value,
                },
            );
        }
        cx.notify();
    }

    pub fn save(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(stamp) = self.stamp.clone().filter(|_| self.dirty() && !self.saving) else {
            return;
        };
        self.saving = true;
        self.error = None;
        let edits = self.edits.clone();
        let job = cx
            .background_executor()
            .spawn(async move { hex_file::save(&path, &stamp, &edits) });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(stamp) => {
                        this.stamp = Some(Arc::new(stamp));
                        this.edits = Arc::default();
                        this.revision += 1;
                    }
                    Err(error) => this.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

struct Page {
    revision: u64,
    offset: u64,
    bytes: Vec<u8>,
}

impl Page {
    fn byte(&self, offset: u64) -> Option<u8> {
        let index = usize::try_from(offset.checked_sub(self.offset)?).ok()?;
        self.bytes.get(index).copied()
    }
}

pub struct HexView {
    pub buffer: Entity<HexBuffer>,
    pub readonly: bool,
    pub palette: Palette,
    focus: FocusHandle,
    offset: Entity<InputState>,
    scrollbar: Entity<SliderState>,
    top_row: u64,
    rows: u64,
    cursor: u64,
    low_nibble: bool,
    wheel: f32,
    len: u64,
    page: Option<Page>,
    loading: Option<(u64, u64)>,
    page_error: Option<(u64, u64, String)>,
    offset_error: Option<&'static str>,
    _subscriptions: [Subscription; 3],
}

impl HexView {
    pub fn new(
        buffer: Entity<HexBuffer>,
        readonly: bool,
        palette: Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let len = buffer.read(cx).source.len();
        let focus = cx.focus_handle();
        let offset = cx.new(|cx| InputState::new(window, cx).placeholder("Hex offset"));
        let scrollbar = cx.new(|_| {
            SliderState::new()
                .max(SCROLL_RANGE)
                .default_value(SCROLL_RANGE)
        });
        let subscriptions = [
            cx.observe_in(&buffer, window, |this, _, window, cx| {
                this.request_page(window, cx);
                cx.notify();
            }),
            cx.subscribe_in(
                &offset,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        let value = input.read(cx).value();
                        let value = value.trim();
                        let value = value
                            .strip_prefix("0x")
                            .or_else(|| value.strip_prefix("0X"))
                            .unwrap_or(value);
                        match u64::from_str_radix(value, 16) {
                            Ok(offset) if offset < this.len => {
                                this.offset_error = None;
                                this.select(offset, window, cx);
                                this.focus.focus(window, cx);
                            }
                            _ => {
                                this.offset_error =
                                    Some("Enter a hexadecimal offset within the file")
                            }
                        }
                        cx.notify();
                    }
                    InputEvent::Change => {
                        this.offset_error = None;
                        cx.notify();
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(&scrollbar, window, |this, _, event, window, cx| {
                let value = match event {
                    SliderEvent::Change(value) | SliderEvent::Release(value) => value.end(),
                };
                let ratio = f64::from((SCROLL_RANGE - value) / SCROLL_RANGE);
                let top = (ratio * this.max_top() as f64).round() as u64;
                this.show_row(top, window, cx);
            }),
        ];
        let mut view = Self {
            buffer,
            readonly,
            palette,
            focus,
            offset,
            scrollbar,
            top_row: 0,
            rows: 1,
            cursor: 0,
            low_nibble: false,
            wheel: 0.,
            len,
            page: None,
            loading: None,
            page_error: None,
            offset_error: None,
            _subscriptions: subscriptions,
        };
        view.request_page(window, cx);
        view
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn max_top(&self) -> u64 {
        self.len.div_ceil(ROW_BYTES).saturating_sub(self.rows)
    }

    fn show_row(&mut self, row: u64, window: &mut Window, cx: &mut Context<Self>) {
        let row = row.min(self.max_top());
        let cursor_row = self.cursor / ROW_BYTES;
        let visible_row = cursor_row.clamp(row, row + self.rows - 1);
        if visible_row != cursor_row {
            self.cursor =
                (visible_row * ROW_BYTES + self.cursor % ROW_BYTES).min(self.len.saturating_sub(1));
            self.low_nibble = false;
        }
        if self.top_row != row {
            self.top_row = row;
            self.request_page(window, cx);
            cx.notify();
        }
    }

    fn update_scrollbar(&self, window: &mut Window, cx: &mut Context<Self>) {
        let ratio = self.top_row as f64 / self.max_top().max(1) as f64;
        let value = SCROLL_RANGE * (1. - ratio as f32);
        self.scrollbar
            .update(cx, |s, cx| s.set_value(value, window, cx));
    }

    fn select(&mut self, offset: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.cursor = offset.min(self.len.saturating_sub(1));
        self.low_nibble = false;
        let row = self.cursor / ROW_BYTES;
        let top = self.top_row.clamp(row.saturating_sub(self.rows - 1), row);
        if top != self.top_row {
            self.show_row(top, window, cx);
            self.update_scrollbar(window, cx);
        }
        cx.notify();
    }

    fn request_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let buffer = self.buffer.read(cx);
        let revision = buffer.revision;
        let offset = self.top_row * ROW_BYTES / PAGE_BYTES * PAGE_BYTES;
        let request = (revision, offset);
        if self.loading.is_some()
            || self
                .page
                .as_ref()
                .is_some_and(|page| (page.revision, page.offset) == request)
            || self
                .page_error
                .as_ref()
                .is_some_and(|&(r, o, _)| (r, o) == request)
            || self.len == 0
        {
            return;
        }
        self.loading = Some(request);
        let source = buffer.source.clone();
        let job = cx
            .background_executor()
            .spawn(async move { source.read(offset, PAGE_BYTES + MAX_ROWS * ROW_BYTES) });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.loading = None;
                if revision == this.buffer.read(cx).revision {
                    match result {
                        Ok(bytes) => {
                            this.page = Some(Page {
                                revision,
                                offset,
                                bytes,
                            });
                            this.page_error = None;
                        }
                        Err(error) => {
                            this.page_error = Some((revision, offset, format!("{error:#}")))
                        }
                    }
                }
                this.request_page(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        if !self.focus.is_focused(window)
            || key.modifiers.control
            || key.modifiers.platform
            || key.modifiers.alt
        {
            return;
        }
        let cursor = match key.key.as_str() {
            "left" => self.cursor.saturating_sub(1),
            "right" => self.cursor.saturating_add(1),
            "up" => self.cursor.saturating_sub(ROW_BYTES),
            "down" => self.cursor.saturating_add(ROW_BYTES),
            "pageup" => self.cursor.saturating_sub(self.rows * ROW_BYTES),
            "pagedown" => self.cursor.saturating_add(self.rows * ROW_BYTES),
            "home" => 0,
            "end" => self.len.saturating_sub(1),
            _ => {
                let mut chars = key.key_char.as_deref().unwrap_or(&key.key).chars();
                if let Some(digit) = chars.next().and_then(|ch| ch.to_digit(16))
                    && chars.next().is_none()
                    && !self.readonly
                {
                    self.edit_digit(digit as u8, window, cx);
                    cx.stop_propagation();
                }
                return;
            }
        };
        self.select(cursor, window, cx);
        cx.stop_propagation();
    }

    fn edit_digit(&mut self, digit: u8, window: &mut Window, cx: &mut Context<Self>) {
        let buffer = self.buffer.read(cx);
        if buffer.saving {
            return;
        }
        let Some(original) = self
            .page
            .as_ref()
            .filter(|page| page.revision == buffer.revision)
            .and_then(|page| page.byte(self.cursor))
        else {
            return;
        };
        let current = buffer
            .edits
            .get(&self.cursor)
            .map_or(original, |edit| edit.after);
        let value = if self.low_nibble {
            (current & 0xf0) | digit
        } else {
            (current & 0x0f) | (digit << 4)
        };
        self.buffer.update(cx, |buffer, cx| {
            buffer.replace_byte(self.cursor, original, value, cx)
        });
        if self.low_nibble {
            self.select(self.cursor + 1, window, cx);
        } else {
            self.low_nibble = true;
            cx.notify();
        }
    }
}

impl Render for HexView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let buffer = self.buffer.read(cx);
        let current_page = self
            .page
            .as_ref()
            .filter(|page| page.revision == buffer.revision);
        let first = self.top_row * ROW_BYTES;
        let end = first.saturating_add(self.rows * ROW_BYTES).min(self.len);
        let widths = match std::env::consts::OS {
            "windows" => "Consolas",
            "macos" => "Menlo",
            _ => "DejaVu Sans Mono",
        };
        let mut rows = div()
            .w(px(548.))
            .flex_shrink_0()
            .font_family(widths)
            .text_size(px(12.));
        for base in (first..end).step_by(ROW_BYTES as usize) {
            let Some(_) = current_page.and_then(|page| page.byte(base)) else {
                break;
            };
            let mut row = div().h(px(ROW_HEIGHT)).flex().items_center().child(
                div()
                    .w(px(121.))
                    .flex_shrink_0()
                    .text_color(p.muted)
                    .child(format!("{base:016X}")),
            );
            let count = ROW_BYTES.min(self.len - base);
            let mut ascii = String::with_capacity(count as usize);
            for offset in base..base + count {
                let Some(original) = current_page.and_then(|page| page.byte(offset)) else {
                    break;
                };
                let edit = buffer.edits.get(&offset);
                let value = edit.map_or(original, |edit| edit.after);
                ascii.push(if value.is_ascii_graphic() || value == b' ' {
                    value as char
                } else {
                    '.'
                });
                let selected = self.cursor == offset;
                row = row.child(
                    div()
                        .id(("hex-byte", offset as usize))
                        .w(px(19.))
                        .flex_shrink_0()
                        .text_center()
                        .cursor_pointer()
                        .when(selected, |cell| cell.bg(p.accent).text_color(p.accent_fg))
                        .when(edit.is_some() && !selected, |cell| {
                            cell.text_color(p.accent)
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.focus.focus(window, cx);
                                this.select(offset, window, cx);
                            }),
                        )
                        .child(if selected && self.low_nibble {
                            format!("{:X}·", value >> 4)
                        } else {
                            format!("{value:02X}")
                        }),
                );
            }
            row = row
                .child(
                    div()
                        .w(px(19.) * (ROW_BYTES - count) as f32)
                        .flex_shrink_0(),
                )
                .child(div().pl(px(8.)).text_color(p.muted).child(ascii));
            rows = rows.child(row);
        }
        let page_error = self
            .page_error
            .as_ref()
            .filter(|&&(revision, offset, _)| {
                revision == buffer.revision && offset == first / PAGE_BYTES * PAGE_BYTES
            })
            .map(|(_, _, text)| text.as_str());
        let error = self.offset_error.or(buffer.error.as_deref()).or(page_error);
        let status = if self.len == 0 {
            "Empty file".into()
        } else if buffer.saving {
            "Saving…".into()
        } else if self.readonly {
            "Read only".into()
        } else if buffer.edits.len() == 1 {
            "1 modified byte · F2 to save".into()
        } else if buffer.dirty() {
            format!("{} modified bytes · F2 to save", buffer.edits.len())
        } else {
            "Type hex digits to overwrite bytes".into()
        };
        let info = format!("0x{:X} / {} bytes · {status}", self.cursor, self.len);
        let weak = cx.weak_entity();
        let rows_now = self.rows;
        let measure = canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                let height = f32::from(bounds.size.height);
                let rows = ((height / ROW_HEIGHT).floor() as u64).clamp(1, MAX_ROWS);
                if rows != rows_now {
                    let weak = weak.clone();
                    window.defer(cx, move |window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.rows = rows;
                            this.show_row(this.top_row, window, cx);
                            this.update_scrollbar(window, cx);
                            cx.notify();
                        });
                    });
                }
            },
        )
        .absolute()
        .size_full();
        div()
            .track_focus(&self.focus)
            .key_context("PreviewPane")
            .on_key_down(cx.listener(Self::key_down))
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(
                div()
                    .h(px(34.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(12.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(p.muted)
                            .child(if self.readonly {
                                "Hex viewer"
                            } else {
                                "Hex editor"
                            }),
                    )
                    .child(div().w(px(166.)).child(Input::new(&self.offset))),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .id("hex-viewport")
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .overflow_x_scroll()
                            .overflow_y_hidden()
                            .restrict_scroll_to_axis()
                            .pl(px(12.))
                            .on_scroll(cx.listener(|this, event: &ScrollEvent, window, cx| {
                                let delta = f32::from(event.delta.pixel_delta(px(ROW_HEIGHT)).y);
                                this.wheel -= delta / ROW_HEIGHT;
                                let lines = this.wheel.trunc() as i64;
                                this.wheel -= lines as f32;
                                if lines != 0 {
                                    this.show_row(
                                        this.top_row.saturating_add_signed(lines),
                                        window,
                                        cx,
                                    );
                                    this.update_scrollbar(window, cx);
                                }
                            }))
                            .child(measure)
                            .when(
                                self.len != 0
                                    && current_page.and_then(|page| page.byte(first)).is_none()
                                    && page_error.is_none(),
                                |view| {
                                    view.child(div().text_color(p.muted).child("Loading bytes…"))
                                },
                            )
                            .child(rows),
                    )
                    .when(self.max_top() > 0, |pane| {
                        pane.child(
                            div()
                                .h_full()
                                .w(px(26.))
                                .py(px(8.))
                                .flex_shrink_0()
                                .flex()
                                .justify_center()
                                .child(
                                    Slider::new(&self.scrollbar)
                                        .vertical()
                                        .h_full()
                                        .accessibility_label("File position")
                                        .tab_stop(false),
                                ),
                        )
                    }),
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
                    .text_color(if error.is_some() { p.error } else { p.muted })
                    .child(error.map(str::to_owned).unwrap_or(info)),
            )
    }
}
