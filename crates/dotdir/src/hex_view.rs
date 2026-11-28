use crate::{
    archive::SourceStamp,
    hex_file::{self, ByteEdit, ByteSource, HexFile, PAGE_BYTES},
    theme::Palette,
};
use gpui_kit::{
    component::{
        IconName, Sizable,
        button::Button,
        input::{Input, InputEvent, InputState},
        menu::{DropdownMenu, PopupMenuItem},
        slider::{Slider, SliderEvent, SliderState},
    },
    prelude::FluentBuilder,
    *,
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

const HEX_ROW_BYTES: u64 = 16;
const ROW_HEIGHT: f32 = 20.;
const FONT_SIZE: f32 = 12.;
const MONO_FONT: &str = if cfg!(target_os = "windows") {
    "Consolas"
} else if cfg!(target_os = "macos") {
    "Menlo"
} else {
    "DejaVu Sans Mono"
};
const MAX_ROWS: u64 = 256;
const MAX_DUMP_COLUMNS: u64 = 1024;
const SCROLL_RANGE: f32 = 1_000_000.;
const INTEGER_WIDTHS: [(&str, usize); 4] = [
    ("1 byte", 1),
    ("2 bytes", 2),
    ("4 bytes", 4),
    ("8 bytes", 8),
];
const MAX_INTEGER_WIDTH: usize = size_of::<u64>();

#[derive(Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    Little,
    Big,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ByteViewMode {
    #[default]
    Hex,
    Dump,
}

impl ByteViewMode {
    fn label(self) -> &'static str {
        match self {
            Self::Hex => "Hex",
            Self::Dump => "Dump",
        }
    }
}

fn ascii_byte(byte: u8) -> char {
    if (b' '..=b'~').contains(&byte) {
        char::from(byte)
    } else {
        '.'
    }
}

fn dump_byte(byte: u8) -> char {
    // DOS screen glyphs for ASCII controls, as used by FAR's console.
    const CONTROLS: [char; 32] = [
        ' ', '☺', '☻', '♥', '♦', '♣', '♠', '•', '◘', '○', '◙', '♂', '♀', '♪', '♫', '☼', '►', '◄',
        '↕', '‼', '¶', '§', '▬', '↨', '↑', '↓', '→', '←', '∟', '↔', '▲', '▼',
    ];
    match byte {
        0..=31 => CONTROLS[usize::from(byte)],
        b' '..=b'~' => char::from(byte),
        0x7f => '⌂',
        _ => '•',
    }
}

fn dump_columns(width: Pixels, cell_width: Pixels) -> u64 {
    ((f32::from(width) / f32::from(cell_width)).floor() as u64).clamp(1, MAX_DUMP_COLUMNS)
}

pub struct HexBuffer {
    source: ByteSource,
    stamp: Option<Arc<SourceStamp>>,
    edits: Arc<BTreeMap<u64, ByteEdit>>,
    revision: u64,
    pub saving: bool,
}

impl HexBuffer {
    pub fn new(file: HexFile) -> Self {
        Self {
            source: file.source,
            stamp: file.stamp.map(Arc::new),
            edits: Arc::default(),
            revision: 0,
            saving: false,
        }
    }

    pub fn dirty(&self) -> bool {
        !self.edits.is_empty()
    }

    pub fn discard(&mut self, cx: &mut Context<Self>) {
        self.edits = Arc::default();
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

    pub fn save(
        &mut self,
        path: PathBuf,
        completed: impl FnOnce(anyhow::Result<()>) + 'static,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(stamp) = self.stamp.clone().filter(|_| self.dirty() && !self.saving) else {
            return false;
        };
        self.saving = true;
        let edits = self.edits.clone();
        let job = cx
            .background_executor()
            .spawn(async move { hex_file::save(&path, &stamp, &edits) });
        cx.spawn(async move |this, cx| {
            let result = job.await.map(Arc::new);
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                if let Ok(stamp) = &result {
                    this.stamp = Some(stamp.clone());
                    this.edits = Arc::default();
                    this.revision += 1;
                }
                cx.notify();
            });
            completed(result.map(|_| ()));
        })
        .detach();
        cx.notify();
        true
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

    fn unsigned_at(
        &self,
        offset: u64,
        width: usize,
        order: ByteOrder,
        edits: &BTreeMap<u64, ByteEdit>,
    ) -> Option<u64> {
        let mut bytes = [0; MAX_INTEGER_WIDTH];
        let value_bytes = bytes.get_mut(..width)?;
        for (index, target) in value_bytes.iter_mut().enumerate() {
            let position = offset.checked_add(index as u64)?;
            *target = edits
                .get(&position)
                .map_or(self.byte(position)?, |edit| edit.after);
        }
        if order == ByteOrder::Big {
            value_bytes.reverse();
        }
        Some(u64::from_le_bytes(bytes))
    }
}

pub struct HexView {
    pub buffer: Entity<HexBuffer>,
    pub readonly: bool,
    pub palette: Palette,
    mode: ByteViewMode,
    focus: FocusHandle,
    offset: Entity<InputState>,
    scrollbar: Entity<SliderState>,
    top_row: u64,
    rows: u64,
    dump_columns: u64,
    dump_left: Pixels,
    cursor: u64,
    integer_width: usize,
    byte_order: ByteOrder,
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
        mode: ByteViewMode,
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
            mode: if readonly { mode } else { ByteViewMode::Hex },
            focus,
            offset,
            scrollbar,
            top_row: 0,
            rows: 1,
            dump_columns: HEX_ROW_BYTES,
            dump_left: px(0.),
            cursor: 0,
            integer_width: 1,
            byte_order: ByteOrder::Little,
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

    pub fn mode(&self) -> ByteViewMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: ByteViewMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.readonly && self.mode != mode {
            let first = self.top_row * self.row_bytes();
            self.mode = mode;
            self.wheel = 0.;
            self.reflow(first, window, cx);
            self.focus.focus(window, cx);
        }
    }

    fn row_bytes(&self) -> u64 {
        match self.mode {
            ByteViewMode::Hex => HEX_ROW_BYTES,
            ByteViewMode::Dump => self.dump_columns,
        }
    }

    fn reflow(&mut self, first: u64, window: &mut Window, cx: &mut Context<Self>) {
        let row_bytes = self.row_bytes();
        let cursor_row = self.cursor / row_bytes;
        self.top_row = (first / row_bytes)
            .clamp(cursor_row.saturating_sub(self.rows - 1), cursor_row)
            .min(self.max_top());
        self.request_page(window, cx);
        self.update_scrollbar(window, cx);
        cx.notify();
    }

    fn max_top(&self) -> u64 {
        self.len
            .div_ceil(self.row_bytes())
            .saturating_sub(self.rows)
    }

    fn show_row(&mut self, row: u64, window: &mut Window, cx: &mut Context<Self>) {
        let row = row.min(self.max_top());
        let row_bytes = self.row_bytes();
        let cursor_row = self.cursor / row_bytes;
        let visible_row = cursor_row.clamp(row, row + self.rows - 1);
        if visible_row != cursor_row {
            self.cursor = visible_row
                .saturating_mul(row_bytes)
                .saturating_add(self.cursor % row_bytes)
                .min(self.len.saturating_sub(1));
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
        let row = self.cursor / self.row_bytes();
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
        let row_bytes = self.row_bytes();
        let first = self.top_row * row_bytes;
        // Keep the whole integer available even at the last visible byte.
        let read_ahead = self.rows * row_bytes + (MAX_INTEGER_WIDTH - 1) as u64;
        let end = first.saturating_add(read_ahead).min(self.len);
        let offset = first / PAGE_BYTES * PAGE_BYTES;
        let request = (revision, offset);
        if self.loading.is_some()
            || self.page.as_ref().is_some_and(|page| {
                (page.revision, page.offset) == request && end - offset <= page.bytes.len() as u64
            })
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
            .spawn(async move { source.read(offset, PAGE_BYTES + read_ahead) });
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
        let row_bytes = self.row_bytes();
        let cursor = match key.key.as_str() {
            "left" => self.cursor.saturating_sub(1),
            "right" => self.cursor.saturating_add(1),
            "up" => self.cursor.saturating_sub(row_bytes),
            "down" => self.cursor.saturating_add(row_bytes),
            "pageup" => self.cursor.saturating_sub(self.rows * row_bytes),
            "pagedown" => self.cursor.saturating_add(self.rows * row_bytes),
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

    fn inspector_dropdown<T: Copy + Eq + 'static>(
        id: &'static str,
        label: &'static str,
        title: &'static str,
        selected: T,
        choices: &'static [(&'static str, T)],
        set: fn(&mut Self, T),
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let view = cx.weak_entity();
        Button::new(id)
            .small()
            .label(title)
            .accessibility_label(format!("{label}, {title}"))
            .icon(IconName::ChevronDown)
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |mut menu, _, _| {
                for &(name, value) in choices {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(name)
                            .checked(value == selected)
                            .on_click(move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    set(this, value);
                                    this.focus.focus(window, cx);
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            })
    }

    fn render_byte_inspector(
        &self,
        page: Option<&Page>,
        edits: &BTreeMap<u64, ByteEdit>,
        cx: &Context<Self>,
    ) -> Div {
        let read = |width| page?.unsigned_at(self.cursor, width, self.byte_order, edits);
        let byte = read(1).map_or_else(
            || "Byte —".into(),
            |value| format!("Byte 0x{value:02X} · {value} dec"),
        );
        let width = self.integer_width;
        let width_label = INTEGER_WIDTHS
            .iter()
            .find_map(|&(label, value)| (value == width).then_some(label))
            .unwrap_or("1 byte");
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_x(px(8.))
            .gap_y(px(2.))
            .child(
                div()
                    .id("byte-inspector-byte")
                    .role(Role::Label)
                    .aria_label(byte.clone())
                    .text_color(self.palette.fg)
                    .child(byte),
            )
            .child(Self::inspector_dropdown(
                "byte-inspector-width",
                "Integer width",
                width_label,
                width,
                &INTEGER_WIDTHS,
                |this, value| this.integer_width = value,
                cx,
            ))
            .when(width > 1, |inspector| {
                let order_label = match self.byte_order {
                    ByteOrder::Little => "Little endian",
                    ByteOrder::Big => "Big endian",
                };
                inspector
                    .child(Self::inspector_dropdown(
                        "byte-inspector-order",
                        "Byte order",
                        order_label,
                        self.byte_order,
                        &[
                            ("Little endian", ByteOrder::Little),
                            ("Big endian", ByteOrder::Big),
                        ],
                        |this, value| this.byte_order = value,
                        cx,
                    ))
                    .when_some(read(width), |inspector, value| {
                        let text = format!("0x{value:0digits$X} · {value} dec", digits = width * 2);
                        inspector.child(
                            div()
                                .id("byte-inspector-integer")
                                .role(Role::Label)
                                .aria_label(text.clone())
                                .text_color(self.palette.fg)
                                .child(text),
                        )
                    })
                    .when(self.len - self.cursor < width as u64, |inspector| {
                        inspector.child(
                            div()
                                .id("byte-inspector-end-of-file")
                                .role(Role::Label)
                                .aria_label("Not enough bytes")
                                .child("Not enough bytes"),
                        )
                    })
            })
    }
}

impl Render for HexView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let dump = self.mode == ByteViewMode::Dump;
        let cell_width = if dump {
            let fonts = window.text_system();
            let mono = fonts.resolve_font(&font(MONO_FONT));
            fonts
                .advance(mono, px(FONT_SIZE), 'M')
                .map_or(px(8.), |size| size.width.max(px(1.)))
        } else {
            px(8.)
        };
        let buffer = self.buffer.read(cx);
        let current_page = self
            .page
            .as_ref()
            .filter(|page| page.revision == buffer.revision);
        let row_bytes = self.row_bytes();
        let first = self.top_row * row_bytes;
        let end = first.saturating_add(self.rows * row_bytes).min(self.len);
        let mut rows = div()
            .when(dump, |s| s.w_full())
            .when(!dump, |s| s.w(px(548.)))
            .flex_shrink_0()
            .font_family(MONO_FONT)
            .text_size(px(FONT_SIZE));
        for base in (first..end).step_by(row_bytes as usize) {
            let Some(_) = current_page.and_then(|page| page.byte(base)) else {
                break;
            };
            let mut row = div()
                .id(("byte-row", base))
                .h(px(ROW_HEIGHT))
                .flex()
                .items_center()
                .when(!dump, |s| {
                    s.child(
                        div()
                            .w(px(121.))
                            .flex_shrink_0()
                            .text_color(p.muted)
                            .child(format!("{base:016X}")),
                    )
                });
            let count = row_bytes.min(self.len - base);
            let mut text = String::with_capacity(count as usize);
            let mut highlights = Vec::new();
            let mut displayed_bytes = 0;
            for offset in base..base + count {
                let Some(original) = current_page.and_then(|page| page.byte(offset)) else {
                    break;
                };
                let edit = buffer.edits.get(&offset);
                let value = edit.map_or(original, |edit| edit.after);
                let selected = self.cursor == offset;
                if dump {
                    let start = text.len();
                    text.push(dump_byte(value));
                    displayed_bytes += 1;
                    if selected || edit.is_some() {
                        highlights.push((
                            start..text.len(),
                            HighlightStyle {
                                color: Some(if selected { p.accent_fg } else { p.accent }),
                                background_color: selected.then_some(p.accent),
                                ..Default::default()
                            },
                        ));
                    }
                    continue;
                }
                text.push(ascii_byte(value));
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
            if dump {
                row = row
                    .w_full()
                    .whitespace_nowrap()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            let x = f32::from(event.position.x - this.dump_left);
                            let column = (x / f32::from(cell_width)).floor() as u64;
                            this.focus.focus(window, cx);
                            this.select(base + column.min(displayed_bytes - 1), window, cx);
                        }),
                    )
                    .child(StyledText::new(text).with_highlights(highlights));
            } else {
                row = row
                    .child(
                        div()
                            .w(px(19.) * (row_bytes - count) as f32)
                            .flex_shrink_0(),
                    )
                    .child(div().pl(px(8.)).text_color(p.muted).child(text));
            }
            rows = rows.child(row);
        }
        let page_error = self
            .page_error
            .as_ref()
            .filter(|&&(revision, offset, _)| {
                revision == buffer.revision && offset == first / PAGE_BYTES * PAGE_BYTES
            })
            .map(|(_, _, text)| text.as_str());
        let error = self.offset_error.or(page_error);
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
        let columns_now = self.dump_columns;
        let left_now = self.dump_left;
        let measure = canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                let height = f32::from(bounds.size.height);
                let rows = ((height / ROW_HEIGHT).floor() as u64).clamp(1, MAX_ROWS);
                let columns = if dump {
                    dump_columns(bounds.size.width, cell_width)
                } else {
                    columns_now
                };
                let left = if dump { bounds.origin.x } else { left_now };
                if rows != rows_now || columns != columns_now || left != left_now {
                    let weak = weak.clone();
                    window.defer(cx, move |window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            let first = this.top_row * this.row_bytes();
                            this.rows = rows;
                            this.dump_columns = columns;
                            this.dump_left = left;
                            this.reflow(first, window, cx);
                        });
                    });
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
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
                    .child(div().text_size(px(12.)).text_color(p.muted).child(if dump {
                        "Dump viewer"
                    } else if self.readonly {
                        "Hex viewer"
                    } else {
                        "Hex editor"
                    }))
                    .child(div().w(px(166.)).child(Input::new(&self.offset)))
                    .when(self.readonly, |bar| {
                        bar.children([ByteViewMode::Hex, ByteViewMode::Dump].map(|mode| {
                            let selected = mode == self.mode;
                            div()
                                .id(("byte-view-mode", mode as usize))
                                .role(Role::Button)
                                .aria_label(mode.label())
                                .aria_selected(selected)
                                .text_size(px(12.))
                                .px(px(6.))
                                .py(px(3.))
                                .rounded(px(3.))
                                .cursor_pointer()
                                .when(selected, |s| {
                                    s.bg(p.selected)
                                        .text_color(p.selected_fg.unwrap_or(p.accent))
                                })
                                .hover(move |s| s.bg(p.selected))
                                .child(mode.label())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.set_mode(mode, window, cx)
                                }))
                        }))
                    }),
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
                            .when(!dump, |view| view.overflow_x_scroll().pl(px(12.)))
                            .when(dump, |view| view.overflow_x_hidden().mx(px(12.)))
                            .overflow_y_hidden()
                            .restrict_scroll_to_axis()
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
                    .when(dump || self.max_top() > 0, |pane| {
                        pane.child(
                            div()
                                .h_full()
                                .w(px(26.))
                                .py(px(8.))
                                .flex_shrink_0()
                                .flex()
                                .justify_center()
                                .when(self.max_top() > 0, |s| {
                                    s.child(
                                        Slider::new(&self.scrollbar)
                                            .vertical()
                                            .h_full()
                                            .accessibility_label("File position")
                                            .tab_stop(false),
                                    )
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px(px(12.))
                    .py(px(4.))
                    .border_t_1()
                    .border_color(p.border)
                    .text_size(px(12.))
                    .text_color(p.muted)
                    .child(
                        div()
                            .min_w_0()
                            .text_ellipsis()
                            .when(error.is_some(), |status| status.text_color(p.error))
                            .child(error.map(str::to_owned).unwrap_or(info)),
                    )
                    .when(self.len > 0, |status| {
                        status.child(self.render_byte_inspector(current_page, &buffer.edits, cx))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{ByteEdit, ByteOrder, PAGE_BYTES, Page, dump_byte, dump_columns};
    use gpui_kit::px;
    use std::collections::BTreeMap;

    #[test]
    fn dump_shows_control_glyphs_and_one_cell_per_byte() {
        let bytes = b"SQLite format 3\0\x10\0\x01\x01\0@\t\r\n\x7f\xb0\xff";
        let display: String = bytes.iter().copied().map(dump_byte).collect();
        assert_eq!(display, "SQLite format 3 ► ☺☺ @○♪◙⌂••");
        assert_eq!(display.chars().count(), bytes.len());
    }

    #[test]
    fn dump_rows_fit_whole_monospace_cells() {
        assert_eq!(dump_columns(px(96.), px(8.)), 12);
        assert_eq!(dump_columns(px(95.), px(8.)), 11);
        assert_eq!(dump_columns(px(1.), px(8.)), 1);
    }

    #[test]
    fn inspector_reads_unsigned_integers_across_page_boundary_and_includes_edits() {
        let start = PAGE_BYTES - 2;
        let mut bytes = vec![0; start as usize];
        bytes.extend_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0xff]);
        let page = Page {
            revision: 0,
            offset: 0,
            bytes,
        };
        let edits = BTreeMap::from([(
            start + 1,
            ByteEdit {
                before: 0x02,
                after: 0x09,
            },
        )]);
        let read = |width, order| page.unsigned_at(start, width, order, &edits);
        assert_eq!(read(1, ByteOrder::Big), Some(1));
        assert_eq!(read(2, ByteOrder::Little), Some(0x0901));
        assert_eq!(read(4, ByteOrder::Big), Some(0x01090304));
        assert_eq!(read(8, ByteOrder::Little), Some(0xff07060504030901));
        assert_eq!(read(8, ByteOrder::Big), Some(0x01090304050607ff));
        assert_eq!(page.unsigned_at(start + 1, 8, ByteOrder::Big, &edits), None);
    }
}
