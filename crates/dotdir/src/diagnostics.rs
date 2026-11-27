use crate::app::{DotDir, TabContent};
use gpui_kit::{
    base::input::{
        DiagnosticSet, DiagnosticSeverity, EditorScrollbarContext, EditorScrollbarRenderer,
        OverviewRulerDecoration, OverviewRulerDecorationCollection, OverviewRulerLane,
        RopeExt as _, TextDecoration, TextDecorationCollection,
    },
    base::{Scrollbar, ScrollbarHandle as _},
    component::{
        ActiveTheme as _,
        input::{EditorState, Rope},
    },
    prelude::FluentBuilder as _,
    *,
};
use std::{cell::RefCell, ops::Range, rc::Rc};

// The height of a point marker painted by GPUI's overview ruler.
const MARK_HEIGHT: Pixels = px(6.);

#[derive(Clone, Copy)]
struct DiagnosticMarker {
    line_start: usize,
    issue_start: usize,
}

pub(crate) fn priority(severity: DiagnosticSeverity) -> u8 {
    match severity {
        DiagnosticSeverity::Error => 0,
        DiagnosticSeverity::Warning => 1,
        DiagnosticSeverity::Info => 2,
        DiagnosticSeverity::Hint => 3,
    }
}

pub(crate) fn color(severity: DiagnosticSeverity, cx: &App) -> Hsla {
    let colors = &cx.theme().highlight_theme.style.status;
    match severity {
        DiagnosticSeverity::Error => colors.error(cx),
        DiagnosticSeverity::Warning => colors.warning(cx),
        DiagnosticSeverity::Info => colors.info(cx),
        DiagnosticSeverity::Hint => colors.hint(cx),
    }
}

pub(crate) fn intersects(range: &Range<usize>, symbol: &Range<usize>) -> bool {
    if symbol.is_empty() {
        range.contains(&symbol.start) || (range.is_empty() && range.start == symbol.start)
    } else if range.is_empty() {
        symbol.contains(&range.start)
    } else {
        range.start < symbol.end && symbol.start < range.end
    }
}

#[derive(Clone, Default)]
pub(crate) struct DiagnosticStyles {
    marks: Vec<OverviewRulerDecoration>,
    targets: Vec<DiagnosticMarker>,
    underlines: Vec<TextDecoration>,
}

impl DiagnosticStyles {
    pub(crate) fn new(text: &Rope, set: &DiagnosticSet, cx: &App) -> Self {
        let mut entries: Vec<_> = set.iter().collect();
        // Earlier text decorations take precedence. Later ruler marks paint on top.
        entries.sort_by_key(|entry| priority(entry.severity));
        let underlines = entries
            .iter()
            .map(|entry| {
                TextDecoration::new(
                    entry.range.clone(),
                    HighlightStyle {
                        underline: Some(UnderlineStyle {
                            color: Some(color(entry.severity, cx)),
                            thickness: px(1.),
                            wavy: true,
                        }),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let targets: Vec<_> = entries
            .iter()
            .map(|entry| DiagnosticMarker {
                line_start: text.line_start_offset(entry.diagnostic.range.start.line as usize),
                issue_start: entry.range.start,
            })
            .collect();
        let marks = entries
            .into_iter()
            .zip(&targets)
            .rev()
            .map(|(entry, target)| {
                let offset = target.line_start;
                OverviewRulerDecoration::new(offset..offset)
                    .with_lane(OverviewRulerLane::Full)
                    .with_color(color(entry.severity, cx))
            })
            .collect();
        Self {
            marks,
            targets,
            underlines,
        }
    }
}

#[derive(Default)]
pub(crate) struct DiagnosticDecorations {
    collections: Option<(OverviewRulerDecorationCollection, TextDecorationCollection)>,
    targets: Rc<RefCell<Vec<DiagnosticMarker>>>,
}

impl DiagnosticDecorations {
    pub(crate) fn set(
        &mut self,
        editor: &Entity<EditorState>,
        styles: DiagnosticStyles,
        cx: &mut App,
    ) {
        if styles.marks.is_empty() {
            self.clear(cx);
            return;
        }
        *self.targets.borrow_mut() = styles.targets;
        if let Some((ruler, underlines)) = &self.collections {
            ruler.set(styles.marks, cx);
            underlines.set(styles.underlines, cx);
        } else {
            self.collections = Some(editor.update(cx, |state, cx| {
                (
                    state.create_overview_ruler_decorations_collection(styles.marks, cx),
                    state.create_decorations_collection(styles.underlines, cx),
                )
            }));
        }
    }

    pub(crate) fn clear(&mut self, cx: &mut App) {
        if !self.targets.borrow().is_empty()
            && let Some((ruler, underlines)) = &self.collections
        {
            self.targets.borrow_mut().clear();
            ruler.clear(cx);
            underlines.clear(cx);
        }
    }

    pub(crate) fn scrollbar_renderer(
        &self,
        editor: &Entity<EditorState>,
        pane: usize,
        document: u64,
        app: WeakEntity<DotDir>,
    ) -> Option<EditorScrollbarRenderer> {
        if self.targets.borrow().is_empty() {
            return None;
        }
        let targets = self.targets.clone();
        let editor = editor.downgrade();
        Some(Rc::new(move |scrollbar, _, _| {
            let viewport = scrollbar.viewport_bounds();
            let placement = scrollbar.placement();
            let stock = Scrollbar::new(&scrollbar)
                .axis(scrollbar.axis())
                .placement(placement)
                .viewport_bounds(viewport);
            let (targets, editor, app) = (targets.clone(), editor.clone(), app.clone());
            div()
                .relative()
                .size_full()
                .child(stock)
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .w(Scrollbar::width())
                        .when(placement.is_left(), |track| track.left_0())
                        .when(!placement.is_left(), |track| track.right_0())
                        .capture_any_mouse_down(move |event, window, cx| {
                            if event.button != MouseButton::Left {
                                return;
                            }
                            let Some(editor) = editor.upgrade() else { return };
                            let Some(offset) = marker_at(
                                &scrollbar,
                                editor.read(cx),
                                &targets.borrow(),
                                event.position.y,
                            ) else { return };
                            window.prevent_default();
                            cx.stop_propagation();
                            let app = app.clone();
                            // Pane activation may swap a mirrored editor for the editable view.
                            window.defer(cx, move |window, cx| {
                                let _ = app.update(cx, |this, cx| {
                                    if this.modal.is_some()
                                        || !matches!(this.panes[pane].tabs[this.panes[pane].active].content,
                                            TabContent::Document(id) if id == document)
                                    {
                                        return;
                                    }
                                    this.focus_pane(pane, window, cx);
                                    this.documents[&document].input.update(cx, |editor, cx| {
                                        let position = editor.text().offset_to_position(offset);
                                        editor.unfold_at(position, cx);
                                        editor.set_cursor_position(position, window, cx);
                                    });
                                });
                            });
                        }),
                )
                .into_any_element()
        }))
    }
}

fn marker_at(
    scrollbar: &EditorScrollbarContext,
    editor: &EditorState,
    targets: &[DiagnosticMarker],
    y: Pixels,
) -> Option<usize> {
    let track = scrollbar.viewport_bounds();
    let line_height = f64::from(f32::from(editor.line_height()?));
    let padding_top = f64::from(f32::from(editor.input_bounds().top() - track.top()));
    let content_height = scrollbar.content_height_f64();
    if content_height <= 0.0 {
        return None;
    }
    let row_y = |row: usize| {
        track.top()
            + track.size.height
                * (((padding_top + line_height * row as f64) / content_height).clamp(0.0, 1.0)
                    as f32)
    };
    let map = editor.display_map();
    let rope = editor.text();
    targets
        .iter()
        .filter_map(|target| {
            let line = rope.offset_to_point(target.line_start).row;
            let (start, end) = if let Some(rows) = map.buffer_line_to_display_row_range(line) {
                let boundary = if rope.line_start_offset(line) == target.line_start {
                    rows.start
                } else {
                    rows.end
                };
                (boundary, boundary)
            } else {
                let row = map.buffer_line_to_display_row(line);
                (row, row + 1)
            };
            let (top, bottom) = (row_y(start), row_y(end));
            let height = (bottom - top).max(MARK_HEIGHT);
            let top = ((top + bottom - height) / 2.)
                .clamp(track.top(), (track.bottom() - height).max(track.top()));
            (top..=top + height)
                .contains(&y)
                .then_some(((y - (top + height / 2.)).abs(), target.issue_start))
        })
        .min_by(|(a, _), (b, _)| f32::from(*a).total_cmp(&f32::from(*b)))
        .map(|(_, offset)| offset)
}
