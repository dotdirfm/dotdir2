use crate::{
    app::{DotDir, Message, Modal},
    archive::Location,
    model::Entry,
    ui::{icon, text},
};
use gpui_kit::{
    component::{IconName, input::Input},
    prelude::FluentBuilder,
    *,
};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Actions,
    Locations,
    Jump { regexp: bool, index: usize },
}

pub(crate) struct PanelMenu {
    pub pane: usize,
    page: Page,
    cursor: usize,
}

impl PanelMenu {
    pub fn is_jump(&self) -> bool {
        matches!(self.page, Page::Jump { .. })
    }
}

enum Action {
    Duplicate,
    Locations,
    Jump,
    Location(Location),
    Bookmark(String),
    EnterPath,
}

struct Item {
    label: SharedString,
    action: Option<Action>,
    section: Option<&'static str>,
}

impl Item {
    fn new(label: impl Into<SharedString>, action: Option<Action>) -> Self {
        Self {
            label: label.into(),
            action,
            section: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum JumpStep {
    First,
    Next,
    Previous,
}

impl DotDir {
    pub(crate) fn open_panel_menu(
        &mut self,
        pane: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.modal {
            Some(Modal::PanelMenu(menu)) if menu.pane == pane => {
                self.dismiss(window, cx);
                return;
            }
            Some(Modal::PanelMenu(_)) | None => {}
            _ => return,
        }
        if self.panels_visible {
            self.dismiss_hover(cx);
            self.show_panel_menu(pane, Page::Actions, 0, window, cx);
        }
    }

    fn show_panel_menu(
        &mut self,
        pane: usize,
        page: Page,
        cursor: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active = pane;
        self.panel_menu_scroll.set_offset(point(px(0.), px(0.)));
        self.show_modal(
            Modal::PanelMenu(PanelMenu { pane, page, cursor }),
            window,
            cx,
        );
        if matches!(page, Page::Jump { .. }) {
            self.jump_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
        }
        window.refresh();
    }

    fn panel_menu_items(&self, menu: &PanelMenu) -> Vec<Item> {
        if menu.page == Page::Actions {
            return vec![
                Item::new("Duplicate Tab", Some(Action::Duplicate)),
                Item::new("Go To", Some(Action::Locations)),
                Item::new(
                    "Jump To…",
                    self.directory(menu.pane).is_some().then_some(Action::Jump),
                ),
            ];
        }
        let opposite = self.pane_location(1 - menu.pane);
        let mut items = vec![
            Item::new(
                "Same Path as Opposite Panel",
                Some(Action::Location(opposite)),
            ),
            Item::new(
                "Home Directory",
                dirs::home_dir().map(|path| Action::Location(path.into())),
            ),
            Item::new("Enter Path…", Some(Action::EnterPath)),
        ];
        for (index, (alias, path)) in self
            .settings
            .path_aliases
            .iter()
            .filter(|(_, path)| !path.trim().is_empty())
            .enumerate()
        {
            let mut item = Item::new(alias.clone(), Some(Action::Bookmark(path.clone())));
            item.section = (index == 0).then_some("Bookmarks");
            items.push(item);
        }
        for (index, root) in self.mounted_roots.iter().enumerate() {
            let mut item = Item::new(
                root.display().to_string(),
                Some(Action::Location(root.clone().into())),
            );
            item.section = (index == 0).then_some("Mounted Drives");
            items.push(item);
        }
        items
    }

    fn select_panel_menu_item(
        &mut self,
        index: usize,
        alternate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::PanelMenu(menu)) = &self.modal else {
            return;
        };
        let pane = menu.pane;
        let Some(action) = self
            .panel_menu_items(menu)
            .into_iter()
            .nth(index)
            .and_then(|item| item.action)
        else {
            return;
        };
        match action {
            Action::Locations => {
                self.show_panel_menu(pane, Page::Locations, 0, window, cx);
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(Message::MountedRoots(mounted_roots()));
                });
            }
            Action::Jump => self.show_panel_menu(
                pane,
                Page::Jump {
                    regexp: false,
                    index: 0,
                },
                0,
                window,
                cx,
            ),
            Action::EnterPath => {
                self.modal = None;
                self.run("location", window, cx);
            }
            Action::Duplicate => {
                self.modal = None;
                self.duplicate_panel_tab(pane, window, cx);
            }
            Action::Location(path) => {
                self.modal = None;
                self.open_panel_location(pane, path, alternate, window, cx);
            }
            Action::Bookmark(value) => {
                self.modal = None;
                match self.resolve_directory(&value, true) {
                    Ok(path) => self.open_panel_location(pane, path, alternate, window, cx),
                    Err(error) => {
                        self.focus_pane(pane, window, cx);
                        self.error(error, cx);
                    }
                }
            }
        }
        window.refresh();
    }

    fn back_from_panel_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Modal::PanelMenu(menu)) = &self.modal {
            let cursor = match menu.page {
                Page::Actions => return,
                Page::Locations => 1,
                Page::Jump { .. } => 2,
            };
            self.show_panel_menu(menu.pane, Page::Actions, cursor, window, cx);
        }
    }

    pub(crate) fn handle_panel_menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(Modal::PanelMenu(menu)) = &self.modal else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if key == "escape" {
            self.dismiss(window, cx);
            window.refresh();
            return true;
        }
        if menu.is_jump() {
            match key {
                "up" => self.select_jump_match(JumpStep::Previous, window, cx),
                "down" => self.select_jump_match(JumpStep::Next, window, cx),
                "enter" => self.confirm_jump(window, cx),
                _ => return false,
            }
            return true;
        }

        if key == "left" && menu.page == Page::Locations {
            self.back_from_panel_menu(window, cx);
            return true;
        }
        let cursor = menu.cursor;
        if key == "enter" || (key == "right" && menu.page == Page::Actions && cursor > 0) {
            self.select_panel_menu_item(cursor, modifiers.shift || modifiers.platform, window, cx);
            return true;
        }
        let items = self.panel_menu_items(menu);
        let enabled = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.action.is_some())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let position = enabled
            .iter()
            .position(|index| *index == cursor)
            .unwrap_or(0);
        let next = match key {
            "up" => (position + enabled.len() - 1) % enabled.len(),
            "down" => (position + 1) % enabled.len(),
            "tab" if modifiers.shift => (position + enabled.len() - 1) % enabled.len(),
            "tab" => (position + 1) % enabled.len(),
            "home" | "pageup" => 0,
            "end" | "pagedown" => enabled.len() - 1,
            _ => return false,
        };
        if let Some(Modal::PanelMenu(menu)) = &mut self.modal {
            menu.cursor = enabled[next];
            self.panel_menu_scroll.scroll_to_item(menu.cursor);
        }
        cx.notify();
        window.refresh();
        true
    }

    pub(crate) fn select_jump_match(
        &mut self,
        step: JumpStep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::PanelMenu(PanelMenu {
            pane,
            page: Page::Jump { regexp, index },
            ..
        })) = &self.modal
        else {
            return;
        };
        let (pane, regexp, index) = (*pane, *regexp, *index);
        let query = self.jump_input.read(cx).value();
        let Some(directory) = self.directory(pane) else {
            return;
        };
        let matches = jump_matches(&directory.entries, query.as_ref(), regexp);
        if matches.is_empty() {
            return;
        }
        let next = match step {
            JumpStep::First => 0,
            JumpStep::Next => (index + 1) % matches.len(),
            JumpStep::Previous => (index + matches.len() - 1) % matches.len(),
        };
        if let Some(Modal::PanelMenu(menu)) = &mut self.modal {
            menu.page = Page::Jump {
                regexp,
                index: next,
            };
        }
        let layout = self.layout_for(pane, window);
        if let Some(directory) = self.directory_mut(pane) {
            directory.cursor = matches[next];
            directory.marked.clear();
            directory.reset_icon_scroll();
            layout.ensure_visible(directory);
        }
        self.update_column_preview(pane);
        self.reveal_column(pane, true);
        cx.notify();
        window.refresh();
    }

    fn confirm_jump(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::PanelMenu(PanelMenu {
            pane,
            page: Page::Jump { regexp, .. },
            ..
        })) = &self.modal
        else {
            return;
        };
        let query = self.jump_input.read(cx).value();
        if self
            .directory(*pane)
            .is_some_and(|d| jump_matches(&d.entries, query.as_ref(), *regexp).contains(&d.cursor))
        {
            self.dismiss(window, cx);
            self.run("open", window, cx);
        }
    }

    pub(crate) fn render_panel_menu(
        &self,
        menu: &PanelMenu,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let right = if menu.pane == 0 {
            window.viewport_size().width / 2. + px(3.)
        } else {
            px(1.)
        };
        let mut surface = div()
            .id("panel-menu-surface")
            .absolute()
            .top(px(29.))
            .right(right)
            .w(px(if menu.is_jump() { 336. } else { 270. }))
            .max_w(relative(0.49))
            .track_focus(&self.modal_focus)
            .key_context("Modal")
            .tab_group()
            .role(if menu.is_jump() {
                Role::Dialog
            } else {
                Role::Menu
            })
            .aria_label(match menu.page {
                Page::Actions => "Panel actions",
                Page::Locations => "Go To",
                Page::Jump { .. } => "Jump To",
            })
            .occlude()
            .bg(p.bg)
            .border_1()
            .border_color(p.border)
            .rounded(px(4.))
            .shadow_lg()
            .p(px(4.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        if let Page::Jump { regexp, .. } = menu.page {
            surface = surface.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .p(px(2.))
                    .child(
                        self.button("jump-back", "‹", Self::back_from_panel_menu, cx)
                            .aria_label("Back"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.jump_input).h(px(26.))),
                    )
                    .child(
                        self.button(
                            "jump-up",
                            "↑",
                            |this, window, cx| {
                                this.select_jump_match(JumpStep::Previous, window, cx)
                            },
                            cx,
                        )
                        .aria_label("Previous match"),
                    )
                    .child(
                        self.button(
                            "jump-down",
                            "↓",
                            |this, window, cx| this.select_jump_match(JumpStep::Next, window, cx),
                            cx,
                        )
                        .aria_label("Next match"),
                    )
                    .child(
                        self.button(
                            "jump-regexp",
                            ".*",
                            |this, window, cx| {
                                if let Some(Modal::PanelMenu(menu)) = &mut this.modal
                                    && let Page::Jump { regexp, .. } = menu.page
                                {
                                    menu.page = Page::Jump {
                                        regexp: !regexp,
                                        index: 0,
                                    };
                                    this.select_jump_match(JumpStep::First, window, cx);
                                    cx.notify();
                                }
                            },
                            cx,
                        )
                        .aria_label("Use regular expression")
                        .when(regexp, |button| button.bg(p.selected)),
                    ),
            );
        } else {
            if menu.page == Page::Locations {
                surface = surface.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(5.))
                        .pb(px(4.))
                        .border_b_1()
                        .border_color(p.border)
                        .child(
                            self.button("locations-back", "‹", Self::back_from_panel_menu, cx)
                                .aria_label("Back"),
                        )
                        .child(text("Go To").font_weight(FontWeight::SEMIBOLD)),
                );
            }
            let mut rows = div()
                .id("panel-menu-rows")
                .track_scroll(&self.panel_menu_scroll)
                .max_h(window.viewport_size().height * 0.65)
                .overflow_y_scroll();
            for (index, item) in self.panel_menu_items(menu).into_iter().enumerate() {
                let enabled = item.action.is_some();
                let opens_submenu = matches!(&item.action, Some(Action::Locations | Action::Jump));
                let opens_location = matches!(
                    &item.action,
                    Some(Action::Location(_) | Action::Bookmark(_))
                );
                let mut row = div()
                    .id(("panel-menu-item", index))
                    .role(Role::MenuItem)
                    .aria_label(item.label.clone())
                    .when(!enabled, |row| {
                        row.aria_description("Unavailable for this tab")
                    })
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(27.))
                    .px(px(8.))
                    .rounded(px(3.))
                    .when(index == menu.cursor && enabled, |row| {
                        row.bg(p.selected).aria_active_descendant()
                    })
                    .text_color(if enabled { p.fg } else { p.muted })
                    .child(text(item.label).flex_1().min_w_0())
                    .when(opens_submenu, |row| {
                        row.child(icon(IconName::ChevronRight, p.muted))
                    });
                if enabled {
                    row = row
                        .cursor_pointer()
                        .on_hover(cx.listener(move |this, hovered, _, cx| {
                            if *hovered
                                && let Some(Modal::PanelMenu(menu)) = &mut this.modal
                                && menu.cursor != index
                            {
                                menu.cursor = index;
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            this.select_panel_menu_item(
                                index,
                                event.modifiers().platform,
                                window,
                                cx,
                            );
                        }));
                }
                if opens_location {
                    row = row.on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.select_panel_menu_item(index, true, window, cx);
                        }),
                    );
                }
                rows = rows.child(
                    div()
                        .when_some(item.section, |group, section| {
                            group.child(
                                text(section)
                                    .px(px(8.))
                                    .pt(px(8.))
                                    .pb(px(3.))
                                    .text_size(px(11.))
                                    .text_color(p.muted),
                            )
                        })
                        .child(row),
                );
            }
            surface = surface.child(rows);
        }
        div()
            .id("panel-menu-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.dismiss(window, cx);
                    window.refresh();
                }),
            )
            .child(surface)
            .into_any_element()
    }
}

fn jump_matches(entries: &[Entry], query: &str, regexp: bool) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let pattern = if regexp {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let Ok(pattern) = regex::RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
    else {
        return Vec::new();
    };
    let mut matches = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.parent && pattern.is_match(&entry.name))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    matches.sort_by_cached_key(|&index| {
        (
            !entries[index].directory,
            entries[index].name.to_lowercase(),
        )
    });
    matches
}

fn mounted_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    let mut roots = (b'A'..=b'Z')
        .map(|drive| PathBuf::from(format!("{}:\\", char::from(drive))))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    #[cfg(not(windows))]
    let mut roots = vec![PathBuf::from("/")];
    #[cfg(target_os = "macos")]
    let mount_dirs = ["/Volumes"];
    #[cfg(all(unix, not(target_os = "macos")))]
    let mount_dirs = ["/mnt", "/media", "/run/media"];
    #[cfg(not(unix))]
    let mount_dirs: [&str; 0] = [];
    roots.extend(
        mount_dirs
            .into_iter()
            .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir()),
    );
    roots.sort_unstable();
    roots.dedup();
    roots
}

#[cfg(test)]
mod tests {
    use super::jump_matches;
    use crate::model::{SortOrder, read_directory};

    #[test]
    fn jump_matches_visible_entries_in_folder_first_name_order() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("Project files")).unwrap();
        for name in [
            "project-02.txt",
            "Project-01.txt",
            ".project-hidden",
            "notes.txt",
        ] {
            std::fs::write(temp.path().join(name), "").unwrap();
        }
        let entries = read_directory(temp.path(), false, SortOrder::Extension).unwrap();
        let names = |query, regexp| {
            jump_matches(&entries, query, regexp)
                .into_iter()
                .map(|i| entries[i].name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(" pROJECT ", false),
            ["Project files", "Project-01.txt", "project-02.txt"]
        );
        assert_eq!(
            names(r"-0[12]\.TXT$", true),
            ["Project-01.txt", "project-02.txt"]
        );
        assert!(names("[", true).is_empty());
        assert!(names("..", false).is_empty());
    }
}
