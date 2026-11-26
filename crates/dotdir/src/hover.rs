use crate::app::{DotDir, TabContent};
use gpui_kit::{
    component::{
        ThemeStyled,
        input::{EditorState, RopeExt},
        text::{TextView, TextViewStyle},
    },
    *,
};
use lsp_types::{HoverContents, MarkedString, MarkupKind, Position};
use std::{cell::Cell, ops::Range, path::Path, path::PathBuf, rc::Rc, time::Duration};
use url::Url;

const SHOW_DELAY: Duration = Duration::from_millis(160);
const HIDE_DELAY: Duration = Duration::from_millis(350);

#[derive(Clone, PartialEq, Eq)]
struct Target {
    pane: usize,
    tab: u64,
    document: u64,
    revision: i32,
    symbol: Range<usize>,
}

struct Hint {
    contents: SharedString,
    markdown: bool,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

#[derive(Default)]
pub(crate) struct HoverHints {
    target: Option<Target>,
    request: Option<Task<()>>,
    hint: Option<Hint>,
    hide: Option<Task<()>>,
}

impl HoverHints {
    pub fn clear(&mut self) -> bool {
        let had_hint = self.hint.is_some();
        *self = Self::default();
        had_hint
    }

    pub fn is_for_document(&self, id: u64) -> bool {
        self.target
            .as_ref()
            .is_some_and(|target| target.document == id)
    }

    pub fn contains(&self, point: Point<Pixels>) -> bool {
        self.hint
            .as_ref()
            .and_then(|hint| hint.bounds.get())
            .is_some_and(|bounds| bounds.contains(&point))
    }
}

impl DotDir {
    fn hover_editor(&self, pane: usize) -> Option<(u64, u64, i32, &Entity<EditorState>)> {
        if !self.panels_visible || self.modal.is_some() {
            return None;
        }
        let tab = self.panes[pane].tabs.get(self.panes[pane].active)?;
        let TabContent::Document(id) = tab.content else {
            return None;
        };
        let document = self.documents.get(&id)?;
        let revision = document.lsp.as_ref()?.revision();
        let editor = if pane != self.active {
            document.inactive_view.as_ref().unwrap_or(&document.input)
        } else {
            &document.input
        };
        Some((tab.id(), id, revision, editor))
    }

    fn hover_editor_for_target(&self, target: &Target) -> Option<&Entity<EditorState>> {
        let (tab, document, revision, editor) = self.hover_editor(target.pane)?;
        (tab == target.tab && document == target.document && revision == target.revision)
            .then_some(editor)
    }

    pub(crate) fn dismiss_hover(&mut self, cx: &mut Context<Self>) -> bool {
        let dismissed = self.hover.clear();
        if dismissed {
            cx.notify();
        }
        dismissed
    }

    fn move_hover(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = &self.hover.target
            && self.hover_editor_for_target(target).is_none()
        {
            self.dismiss_hover(cx);
        }

        if self.hover.hint.is_some() {
            let on_symbol = self
                .hover
                .target
                .as_ref()
                .and_then(|target| {
                    self.hover_editor_for_target(target)?
                        .read(cx)
                        .range_to_bounds(&target.symbol)
                })
                .is_some_and(|bounds| bounds.contains(&position));
            if self.hover.contains(position) || on_symbol {
                self.hover.hide = None;
            } else if self.hover.hide.is_none() {
                self.hover.hide = Some(cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(HIDE_DELAY).await;
                    let _ = cx.update(|window, cx| {
                        let _ = this.update(cx, |this, cx| {
                            this.dismiss_hover(cx);
                            this.move_hover(window.mouse_position(), window, cx);
                        });
                    });
                }));
            }
            return;
        }

        let hit = [self.active, 1 - self.active].into_iter().find_map(|pane| {
            let (tab, document, revision, editor) = self.hover_editor(pane)?;
            if !editor.read(cx).input_bounds().contains(&position) {
                return None;
            }
            let (symbol, offset) = editor.update(cx, |editor, cx| {
                let index = editor.character_index_for_point(position, window, cx)?;
                let text = editor.text();
                let offset = text.utf16_to_byte_idx(index);
                let symbol = text.word_range(offset).or_else(|| {
                    let ch = text.char_at(offset)?;
                    (!ch.is_whitespace()).then_some(offset..offset + ch.len_utf8())
                })?;
                let bounds = editor.range_to_bounds(&symbol)?;
                let offset = if offset >= symbol.end {
                    symbol.start
                } else {
                    offset
                };
                bounds.contains(&position).then_some((symbol, offset))
            })?;
            Some((
                Target {
                    pane,
                    tab,
                    document,
                    revision,
                    symbol,
                },
                offset,
            ))
        });
        if hit.as_ref().map(|(target, _)| target) == self.hover.target.as_ref() {
            return;
        }
        self.hover.clear();
        let Some((target, offset)) = hit else {
            return;
        };
        self.hover.target = Some(target.clone());
        self.hover.request = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SHOW_DELAY).await;
            let Some(request) = this
                .read_with(cx, |this, cx| {
                    let text = this.hover_editor_for_target(&target)?.read(cx).text();
                    let buffer = this.documents.get(&target.document)?.lsp.as_ref()?;
                    Some(buffer.hover(text, offset, cx))
                })
                .ok()
                .flatten()
            else {
                return;
            };
            let Ok(Some(hover)) = request.await else {
                return;
            };
            let (markdown, contents) = hover_text(hover.contents);
            if contents.trim().is_empty() {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                if this.hover.target.as_ref() == Some(&target)
                    && this.hover_editor_for_target(&target).is_some()
                {
                    this.hover.hint = Some(Hint {
                        contents: contents.into(),
                        markdown,
                        bounds: Rc::default(),
                    });
                    cx.notify();
                }
            });
        }));
    }

    pub(crate) fn render_hover(&mut self, window: &Window, cx: &Context<Self>) -> Vec<AnyElement> {
        if !window.is_window_active()
            || (self.hover_editor(0).is_none() && self.hover_editor(1).is_none())
        {
            self.hover.clear();
            return Vec::new();
        }
        if let Some(target) = &self.hover.target
            && self.hover_editor_for_target(target).is_none()
        {
            self.hover.clear();
        }
        let weak = cx.weak_entity();
        // Capture movement over both the editor and the deferred, occluding hint.
        let observer = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let view = weak.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Capture {
                        let _ = view.update(cx, |this, cx| {
                            if event.pressed_button.is_none() || this.hover.hint.is_some() {
                                this.move_hover(event.position, window, cx);
                            }
                        });
                    }
                });
                let view = weak.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture {
                        let _ = view.update(cx, |this, cx| {
                            if !this.hover.contains(event.position) {
                                this.dismiss_hover(cx);
                            }
                        });
                    }
                });
                let view = weak.clone();
                window.on_mouse_event(move |event: &ScrollEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture {
                        let _ = view.update(cx, |this, cx| {
                            if !this.hover.contains(event.position) {
                                this.dismiss_hover(cx);
                            }
                        });
                    }
                });
                window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture {
                        let _ = weak.update(cx, |this, cx| this.dismiss_hover(cx));
                    }
                });
            },
        )
        .absolute()
        .size_0()
        .into_any_element();

        let Some(popup) = self.hover_popup(window, cx) else {
            return vec![observer];
        };
        vec![observer, popup]
    }

    fn hover_popup(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let hint = self.hover.hint.as_ref()?;
        let target = self.hover.target.as_ref()?;
        let editor = self.hover_editor_for_target(target)?.read(cx);
        let trigger = editor.range_to_bounds(&target.symbol)?;
        let source = self.documents[&target.document]
            .path
            .local()
            .map(Path::to_path_buf);
        let view = cx.weak_entity();
        let origin = target.clone();
        let content = if hint.markdown {
            TextView::markdown("lsp-hover-markdown", hint.contents.clone())
                .style(
                    TextViewStyle::default()
                        .paragraph_gap(rems(0.5))
                        .heading_font_size(|level, rem_size| match level {
                            1..=3 => rem_size,
                            4 => rem_size * 0.9,
                            _ => rem_size * 0.8,
                        })
                        .code_block(StyleRefinement::default().px_1().py_0().text_size(px(11.))),
                )
                .code_block_actions(|_, _, _| div().size_0())
                .selectable(true)
                .on_link_click(move |href, _, window, cx| {
                    let Some(link) = source
                        .as_deref()
                        .and_then(|source| hover_link(source, href.as_ref()))
                    else {
                        return;
                    };
                    let view = view.clone();
                    let origin = origin.clone();
                    window.defer(cx, move |window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if this.hover.target.as_ref() != Some(&origin)
                                || this.hover_editor_for_target(&origin).is_none()
                            {
                                return;
                            }
                            this.dismiss_hover(cx);
                            match link {
                                HoverLink::Source(path, position) => {
                                    this.open_source_location(
                                        origin.pane,
                                        path,
                                        position,
                                        window,
                                        cx,
                                    );
                                }
                                HoverLink::Web(url) => cx.open_url(url.as_str()),
                            }
                        });
                    });
                })
                .into_any_element()
        } else {
            div().child(hint.contents.clone()).into_any_element()
        };

        let bounds = hint.bounds.clone();
        let margin = px(8.);
        let viewport = window.viewport_size();
        let popup = div()
            .id("lsp-hover")
            .occlude()
            .popover_style(cx)
            .shadow_md()
            .p_2()
            .text_xs()
            .max_w((viewport.width - margin * 2).min(px(500.)))
            .max_h((viewport.height - margin * 2).min(px(320.)))
            .overflow_y_scroll()
            .child(
                canvas(move |area, _, _| bounds.set(Some(area)), |_, _, _, _| {})
                    .absolute()
                    .inset_0(),
            )
            .child(content);
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::TopLeft)
                    .attach(trigger, Anchor::BottomLeft)
                    .offset(point(px(0.), px(3.)))
                    .snap_to_window_with_margin(Edges::all(margin))
                    .child(popup),
            )
            .with_priority(base::POPUP_PRIORITY)
            .into_any_element(),
        )
    }
}

fn hover_text(contents: HoverContents) -> (bool, String) {
    fn marked_text(item: MarkedString) -> String {
        match item {
            MarkedString::String(markdown) => markdown,
            MarkedString::LanguageString(code) => {
                let mut fence = "~~~".to_string();
                while code.value.contains(&fence) {
                    fence.push('~');
                }
                format!("{fence}{}\n{}\n{fence}", code.language, code.value)
            }
        }
    }

    match contents {
        HoverContents::Markup(markup) => (markup.kind == MarkupKind::Markdown, markup.value),
        HoverContents::Scalar(item) => (true, marked_text(item)),
        HoverContents::Array(items) => (
            true,
            items
                .into_iter()
                .map(marked_text)
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HoverLink {
    Source(PathBuf, Option<Position>),
    Web(Url),
}

fn hover_link(source: &Path, href: &str) -> Option<HoverLink> {
    let base = Url::from_file_path(source).ok()?;
    let url = base.join(href).ok()?;
    match url.scheme() {
        "file" if url.host_str().is_none() => Some(HoverLink::Source(
            url.to_file_path().ok()?,
            url.fragment().and_then(source_position),
        )),
        "https" | "http" => Some(HoverLink::Web(url)),
        _ => None,
    }
}

fn source_position(fragment: &str) -> Option<Position> {
    // TypeScript emits 1-based ranges such as #194,10-194,13. Also
    // recognize line and column anchors such as #L194 and #L194C10.
    let start = fragment.split('-').next()?.trim_start_matches('L');
    let (line, column) = match start.split_once([',', 'C']) {
        Some((line, column)) => (line.parse::<u32>().ok()?, column.parse::<u32>().ok()?),
        None => (start.parse::<u32>().ok()?, 1),
    };
    Some(Position::new(line.checked_sub(1)?, column.checked_sub(1)?))
}

#[cfg(test)]
mod tests {
    use super::{HoverLink, hover_link};
    use lsp_types::Position;
    use url::Url;

    #[test]
    fn typescript_documentation_links_resolve_to_editor_positions() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("source.ts");
        let docs = Url::from_directory_path(workspace.path())
            .unwrap()
            .join("node_modules/%40types/react/index.d.ts#194,10-194,13")
            .unwrap();
        assert_eq!(
            hover_link(&source, docs.as_str()),
            Some(HoverLink::Source(
                workspace
                    .path()
                    .join("node_modules/@types/react/index.d.ts"),
                Some(Position::new(193, 9)),
            )),
        );
        assert_eq!(
            hover_link(&source, "other%20source.ts#L8C4"),
            Some(HoverLink::Source(
                workspace.path().join("other source.ts"),
                Some(Position::new(7, 3))
            )),
        );
        assert_eq!(
            hover_link(&source, "other.ts#unknown"),
            Some(HoverLink::Source(workspace.path().join("other.ts"), None)),
        );
    }

    #[test]
    fn hover_links_only_open_local_sources_or_web_urls() {
        let source = std::env::current_dir().unwrap().join("source.ts");
        let url = Url::parse("https://react.dev/reference/react").unwrap();
        assert_eq!(hover_link(&source, url.as_str()), Some(HoverLink::Web(url)));
        assert_eq!(hover_link(&source, "command:workbench.action"), None);
        assert_eq!(hover_link(&source, "file://remote.test/source.ts"), None);
    }
}
