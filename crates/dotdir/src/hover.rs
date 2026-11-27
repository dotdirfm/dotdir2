use crate::{
    app::{DotDir, TabContent},
    diagnostics,
};
use gpui_kit::{
    base::input::{DiagnosticSet, DiagnosticSeverity},
    component::{
        ThemeStyled,
        input::{EditorState, RopeExt},
        text::{TextView, TextViewStyle},
    },
    prelude::FluentBuilder as _,
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
    session: u64,
    revision: i32,
    symbol: Range<usize>,
}

struct Documentation {
    contents: SharedString,
    markdown: bool,
}

#[derive(PartialEq, Eq)]
struct HoverDiagnostic {
    message: SharedString,
    severity: DiagnosticSeverity,
}

#[derive(Default)]
struct Hint {
    documentation: Option<Documentation>,
    diagnostics: Vec<HoverDiagnostic>,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl Hint {
    fn visible(&self) -> bool {
        !self.diagnostics.is_empty() || self.documentation.is_some()
    }
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
        let had_hint = self.visible();
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
            .filter(|hint| hint.visible())
            .and_then(|hint| hint.bounds.get())
            .is_some_and(|bounds| bounds.contains(&point))
    }

    fn visible(&self) -> bool {
        self.hint.as_ref().is_some_and(Hint::visible)
    }
}

impl DotDir {
    fn hover_editor(&self, pane: usize) -> Option<(u64, u64, u64, i32, &Entity<EditorState>)> {
        if !self.panels_visible || self.modal.is_some() {
            return None;
        }
        let tab = self.panes[pane].tabs.get(self.panes[pane].active)?;
        let TabContent::Document(id) = tab.content else {
            return None;
        };
        let document = self.documents.get(&id)?;
        let buffer = document.lsp.as_ref()?;
        let editor = if pane != self.active {
            document.inactive_editor().unwrap_or(&document.input)
        } else {
            &document.input
        };
        Some((tab.id(), id, buffer.session(), buffer.revision(), editor))
    }

    fn hover_editor_for_target(&self, target: &Target) -> Option<&Entity<EditorState>> {
        let (tab, document, session, revision, editor) = self.hover_editor(target.pane)?;
        (tab == target.tab
            && document == target.document
            && session == target.session
            && revision == target.revision)
            .then_some(editor)
    }

    fn diagnostics_for_hover(&self, target: &Target) -> Vec<HoverDiagnostic> {
        let Some(buffer) = self
            .documents
            .get(&target.document)
            .and_then(|doc| doc.lsp.as_ref())
            .filter(|buffer| {
                buffer.session() == target.session && buffer.revision() == target.revision
            })
        else {
            return Vec::new();
        };
        let diagnostics = buffer.diagnostics();
        diagnostics
            .as_ref()
            .map_or_else(Vec::new, |set| hover_diagnostics(set, &target.symbol))
    }

    pub(crate) fn refresh_hover_diagnostics(&mut self, document: u64) {
        let Some(target) = self
            .hover
            .target
            .as_ref()
            .filter(|target| target.document == document)
        else {
            return;
        };
        if self.hover.hint.is_some() {
            let diagnostics = self.diagnostics_for_hover(target);
            if let Some(hint) = &mut self.hover.hint {
                hint.diagnostics = diagnostics;
            }
        }
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

        if self.hover.visible() {
            let on_symbol = self
                .hover
                .target
                .as_ref()
                .and_then(|target| {
                    hover_trigger(
                        self.hover_editor_for_target(target)?.read(cx),
                        &target.symbol,
                    )
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
            let (tab, document, session, revision, editor) = self.hover_editor(pane)?;
            if !editor.read(cx).input_bounds().contains(&position) {
                return None;
            }
            let buffer = self.documents.get(&document)?.lsp.as_ref()?;
            let (symbol, offset) = editor.update(cx, |editor, cx| {
                let index = editor.character_index_for_point(position, window, cx)?;
                let text = editor.text();
                let offset = text.utf16_to_byte_idx(index);
                if let Some(word) = text.word_range(offset)
                    && hover_trigger(editor, &word).is_some_and(|bounds| bounds.contains(&position))
                {
                    let hover_offset = if word.contains(&offset) {
                        offset
                    } else {
                        word.start
                    };
                    return Some((word, Some(hover_offset)));
                }
                let ch = text.char_at(offset);
                let span = offset..offset + ch.map_or(0, char::len_utf8);
                if !hover_trigger(editor, &span)?.contains(&position) {
                    return None;
                }
                if ch.is_some_and(|ch| !ch.is_whitespace()) {
                    return Some((span, Some(offset)));
                }
                // Diagnostics can point at whitespace or the end of the buffer.
                buffer
                    .diagnostics()
                    .as_ref()?
                    .iter()
                    .take_while(|entry| entry.range.start <= span.start)
                    .any(|entry| diagnostics::intersects(&entry.range, &span))
                    .then_some((span, None))
            })?;
            Some((
                Target {
                    pane,
                    tab,
                    document,
                    session,
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
                .update(cx, |this, cx| {
                    if this.hover.target.as_ref() != Some(&target) {
                        return None;
                    }
                    let text = this.hover_editor_for_target(&target)?.read(cx).text();
                    let buffer = this.documents.get(&target.document)?.lsp.as_ref()?;
                    let request = offset.map(|offset| buffer.hover(text, offset, cx));
                    this.hover.hint = Some(Hint {
                        diagnostics: this.diagnostics_for_hover(&target),
                        ..Default::default()
                    });
                    if this.hover.visible() {
                        cx.notify();
                    }
                    request
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
                    if let Some(hint) = &mut this.hover.hint {
                        hint.documentation = Some(Documentation {
                            contents: contents.into(),
                            markdown,
                        });
                        cx.notify();
                    }
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
                            if event.pressed_button.is_none() || this.hover.visible() {
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
        let hint = self.hover.hint.as_ref().filter(|hint| hint.visible())?;
        let target = self.hover.target.as_ref()?;
        let editor = self.hover_editor_for_target(target)?.read(cx);
        let trigger = hover_trigger(editor, &target.symbol)?;
        let source = self.documents[&target.document]
            .path
            .local()
            .map(Path::to_path_buf);
        let view = cx.weak_entity();
        let origin = target.clone();
        let documentation = hint.documentation.as_ref().map(|documentation| {
            if documentation.markdown {
                TextView::markdown("lsp-hover-markdown", documentation.contents.clone())
                    .style(
                        TextViewStyle::default()
                            .paragraph_gap(rems(0.5))
                            .heading_font_size(|level, rem_size| match level {
                                1..=3 => rem_size,
                                4 => rem_size * 0.9,
                                _ => rem_size * 0.8,
                            })
                            .code_block(
                                StyleRefinement::default().px_1().py_0().text_size(px(11.)),
                            ),
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
                div()
                    .whitespace_normal()
                    .child(documentation.contents.clone())
                    .into_any_element()
            }
        });
        let messages = hint
            .diagnostics
            .iter()
            .enumerate()
            .map(|(index, diagnostic)| {
                let severity = match diagnostic.severity {
                    DiagnosticSeverity::Error => "Error",
                    DiagnosticSeverity::Warning => "Warning",
                    DiagnosticSeverity::Info => "Information",
                    DiagnosticSeverity::Hint => "Hint",
                };
                div()
                    .id(("hover-diagnostic", index))
                    .role(Role::Label)
                    .aria_label(format!("{severity} — {}", diagnostic.message))
                    .whitespace_normal()
                    .text_color(diagnostics::color(diagnostic.severity, cx))
                    .children(
                        diagnostic
                            .message
                            .lines()
                            .map(|line| div().child(line.to_owned())),
                    )
            });

        let bounds = hint.bounds.clone();
        let margin = px(8.);
        let viewport = window.viewport_size();
        let popup = div()
            .id("lsp-hover")
            .role(Role::Tooltip)
            .occlude()
            .popover_style(cx)
            .shadow_md()
            .flex()
            .flex_col()
            .gap_2()
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
            .when(!hint.diagnostics.is_empty(), |popup| {
                popup.child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_shrink_0()
                        .gap_1()
                        .children(messages),
                )
            })
            .children(documentation.map(|content| {
                div()
                    .flex_shrink_0()
                    .when(!hint.diagnostics.is_empty(), |section| {
                        section
                            .border_t_1()
                            .border_color(self.palette.border)
                            .pt_2()
                    })
                    .child(content)
            }));
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

fn hover_diagnostics(set: &DiagnosticSet, symbol: &Range<usize>) -> Vec<HoverDiagnostic> {
    let mut messages: Vec<_> = set
        .iter()
        .take_while(|entry| entry.range.start <= symbol.end)
        .filter(|entry| diagnostics::intersects(&entry.range, symbol))
        .map(|entry| HoverDiagnostic {
            message: entry.message.clone(),
            severity: entry.severity,
        })
        .collect();
    messages.sort_by_key(|entry| diagnostics::priority(entry.severity));
    messages
}

fn hover_trigger(editor: &EditorState, symbol: &Range<usize>) -> Option<Bounds<Pixels>> {
    let mut bounds = editor.range_to_bounds(symbol)?;
    if bounds.size.width == px(0.) {
        bounds.size.width = px(4.);
    }
    Some(bounds)
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
    use super::{DiagnosticSet, DiagnosticSeverity, HoverLink, hover_diagnostics, hover_link};
    use gpui_kit::{base::input::Diagnostic, component::input::Rope};
    use lsp_types::Position;
    use url::Url;

    #[test]
    fn hover_shows_only_overlapping_diagnostics_in_severity_order() {
        let mut set = DiagnosticSet::new(&Rope::from("prefix word neighbour"));
        for (start, end, severity, message) in [
            (0, 21, DiagnosticSeverity::Info, "Outer information"),
            (0, 6, DiagnosticSeverity::Error, "Earlier error"),
            (7, 11, DiagnosticSeverity::Warning, "Word warning"),
            (8, 9, DiagnosticSeverity::Error, "Word error"),
            (9, 9, DiagnosticSeverity::Hint, "Insertion point"),
            (11, 11, DiagnosticSeverity::Error, "Next boundary"),
        ] {
            set.push(
                Diagnostic::new(Position::new(0, start)..Position::new(0, end), message)
                    .with_severity(severity),
            );
        }
        let messages: Vec<_> = hover_diagnostics(&set, &(7..11))
            .iter()
            .map(|diagnostic| diagnostic.message.to_string())
            .collect();
        assert_eq!(
            messages,
            [
                "Word error",
                "Word warning",
                "Outer information",
                "Insertion point"
            ]
        );
        assert!(
            hover_diagnostics(&set, &(11..11))
                .iter()
                .any(|item| item.message == "Next boundary")
        );
    }

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
