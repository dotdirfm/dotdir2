use gpui_kit::{
    App, Entity, HighlightStyle, Hsla, UnderlineStyle,
    base::input::{
        DiagnosticSet, DiagnosticSeverity, OverviewRulerDecoration,
        OverviewRulerDecorationCollection, OverviewRulerLane, RopeExt as _, TextDecoration,
        TextDecorationCollection,
    },
    component::{
        ActiveTheme as _,
        input::{EditorState, Rope},
    },
    px,
};
use std::ops::Range;

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
        let marks = entries
            .into_iter()
            .rev()
            .map(|entry| {
                let offset = text.line_start_offset(entry.diagnostic.range.start.line as usize);
                OverviewRulerDecoration::new(offset..offset)
                    .with_lane(OverviewRulerLane::Full)
                    .with_color(color(entry.severity, cx))
            })
            .collect();
        Self { marks, underlines }
    }
}

#[derive(Default)]
pub(crate) struct DiagnosticDecorations {
    collections: Option<(OverviewRulerDecorationCollection, TextDecorationCollection)>,
    populated: bool,
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
        self.populated = true;
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
        if std::mem::take(&mut self.populated)
            && let Some((ruler, underlines)) = &self.collections
        {
            ruler.clear(cx);
            underlines.clear(cx);
        }
    }
}
