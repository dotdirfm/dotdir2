use gpui_kit::{
    App, Entity,
    base::input::{
        DiagnosticSeverity, OverviewRulerDecoration, OverviewRulerDecorationCollection,
        OverviewRulerLane, RopeExt as _,
    },
    component::{ActiveTheme as _, input::EditorState},
};

#[derive(Default)]
pub(crate) struct DiagnosticRuler(Option<OverviewRulerDecorationCollection>);

impl DiagnosticRuler {
    pub(crate) fn set(
        &mut self,
        editor: &Entity<EditorState>,
        marks: Vec<OverviewRulerDecoration>,
        cx: &mut App,
    ) {
        if marks.is_empty() {
            self.clear(cx);
        } else if let Some(collection) = &self.0 {
            collection.set(marks, cx);
        } else {
            self.0 = Some(editor.update(cx, |state, cx| {
                state.create_overview_ruler_decorations_collection(marks, cx)
            }));
        }
    }

    pub(crate) fn clear(&mut self, cx: &mut App) {
        if let Some(collection) = self.0.take() {
            collection.dispose(cx);
        }
    }
}

pub(crate) fn marks(state: &EditorState, cx: &App) -> Vec<OverviewRulerDecoration> {
    let Some(diagnostics) = state.diagnostics().filter(|set| !set.is_empty()) else {
        return Vec::new();
    };
    let colors = &cx.theme().highlight_theme.style.status;
    let colors = [
        colors.hint(cx),
        colors.info(cx),
        colors.warning(cx),
        colors.error(cx),
    ];
    let mut marks: Vec<_> = diagnostics
        .iter()
        .map(|entry| {
            let severity = match entry.severity {
                DiagnosticSeverity::Hint => 0,
                DiagnosticSeverity::Info => 1,
                DiagnosticSeverity::Warning => 2,
                DiagnosticSeverity::Error => 3,
            };
            (entry.diagnostic.range.start.line as usize, severity)
        })
        .collect();
    // Draw stronger diagnostics last when they share an editor row.
    marks.sort_by_key(|&(_, severity)| severity);
    marks
        .into_iter()
        .map(|(line, severity)| {
            let offset = state.text().line_start_offset(line);
            OverviewRulerDecoration::new(offset..offset)
                .with_lane(OverviewRulerLane::Full)
                .with_color(colors[severity])
        })
        .collect()
}
