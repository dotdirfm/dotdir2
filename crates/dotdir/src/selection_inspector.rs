use crate::{
    app::{DotDir, Tab, TabContent},
    model::Entry,
};
use gpui_kit::{Context, Window};

pub(crate) const TITLE: &str = "Selection Inspector";
pub(crate) const SUMMARY_TITLE: &str = "Selection: Show Summary";
pub(crate) const INSPECT: &str = "selection.inspect";
pub(crate) const SUMMARY: &str = "selection.summary";

#[derive(Clone)]
pub(crate) struct SelectionSnapshot {
    pub path: String,
    pub entries: Vec<Entry>,
}

impl SelectionSnapshot {
    pub fn summary(&self) -> String {
        format!("{} item(s) selected in {}", self.entries.len(), self.path)
    }
}

impl DotDir {
    pub(crate) fn selection_snapshot(&self) -> SelectionSnapshot {
        let (pane, entries) = self.extension_selection();
        let location = self.pane_location(pane);
        let path = location
            .local()
            .map_or_else(|| location.display(), crate::archive::display_local_path);
        SelectionSnapshot { path, entries }
    }

    pub(crate) fn open_selection_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.selection_snapshot();
        let existing = self.panes.iter().enumerate().find_map(|(pane, panel)| {
            panel
                .tabs
                .iter()
                .position(|tab| matches!(tab.content, TabContent::SelectionInspector { .. }))
                .map(|index| (pane, index))
        });
        let (pane, index) = if let Some((pane, index)) = existing {
            if let TabContent::SelectionInspector {
                snapshot: current, ..
            } = &mut self.panes[pane].tabs[index].content
            {
                *current = snapshot;
            }
            (pane, index)
        } else {
            let pane = 1 - self.active;
            let id = self.id();
            let tabs = &mut self.panes[pane].tabs;
            tabs.push(Tab::new(TabContent::SelectionInspector { id, snapshot }));
            (pane, tabs.len() - 1)
        };
        self.set_tab(pane, index, window, cx);
    }

    pub(crate) fn refresh_selection_inspector(&mut self, pane: usize, cx: &mut Context<Self>) {
        let snapshot = self.selection_snapshot();
        let panel = &mut self.panes[pane];
        if let TabContent::SelectionInspector {
            snapshot: current, ..
        } = &mut panel.tabs[panel.active].content
        {
            *current = snapshot;
            cx.notify();
        }
    }
}
