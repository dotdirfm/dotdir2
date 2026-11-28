use crate::{app::DotDir, lsp::Buffer};
use gpui_kit::{
    component::{
        Icon,
        input::{EditorState, Rope, RopeExt, ToggleCodeActions},
    },
    *,
};
use std::{
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};

const PROBE_DELAY: Duration = Duration::from_millis(250);
const BUTTON_SIZE: Pixels = px(16.);
pub(crate) const GUTTER_WIDTH: Pixels = px(18.);
pub(crate) const ICON_PATH: &str = "icons/lightbulb.svg";

pub(crate) fn action_range(buffer: &Buffer, text: &Rope, selection: Range<usize>) -> Range<usize> {
    if !selection.is_empty() {
        return selection;
    }
    let row = text.offset_to_point(selection.start).row;
    let start = text.line_start_offset(row);
    let end = text.line_end_offset(row);
    if buffer.diagnostics().as_ref().is_some_and(|set| {
        let line = start..end + 1;
        set.range(line.clone())
            .any(|entry| crate::diagnostics::intersects(&entry.range, &line))
    }) {
        start..end
    } else {
        selection
    }
}

#[derive(PartialEq, Eq)]
struct Target {
    document: u64,
    session: u64,
    generation: u64,
    range: Range<usize>,
}

#[derive(Default)]
pub(crate) struct QuickActions {
    pending: Option<(Target, Instant)>,
    requested: bool,
    visible: bool,
    task: Option<Task<()>>,
}

impl DotDir {
    pub(crate) fn update_quick_actions(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let candidate = self
            .active_document()
            .and_then(|id| self.documents.get(&id))
            .filter(|doc| self.panels_visible && self.modal.is_none() && !doc.readonly)
            .and_then(|doc| {
                let editor = doc.input.read(cx);
                let buffer = doc.lsp.as_ref()?;
                if editor.lsp().code_action_providers.is_empty()
                    || !editor.focus_handle(cx).is_focused(window)
                    || doc
                        .lsp_status
                        .as_deref()
                        .is_none_or(|status| status.starts_with("Starting "))
                {
                    return None;
                }
                let target = Target {
                    document: doc.id,
                    session: buffer.session(),
                    generation: buffer.action_generation(),
                    range: action_range(buffer, editor.text(), editor.selected_range()),
                };
                Some((target, buffer.clone(), doc.input.clone()))
            });
        let actions = &mut self.quick_actions;
        let Some((target, buffer, editor)) = candidate else {
            let changed = actions.visible;
            *actions = QuickActions::default();
            return changed;
        };
        if actions
            .pending
            .as_ref()
            .is_none_or(|(current, _)| *current != target)
        {
            actions.pending = Some((target, Instant::now()));
            actions.requested = false;
            actions.task = None;
        }
        let (target, since) = actions.pending.as_ref().unwrap();
        let visible = buffer.has_code_actions(&target.range);
        let changed = visible != actions.visible;
        actions.visible = visible;
        if !visible && !actions.requested && since.elapsed() >= PROBE_DELAY {
            actions.requested = true;
            let request =
                buffer.list_code_actions(editor.read(cx).text(), target.range.clone(), cx);
            actions.task = Some(cx.spawn(async move |_, _| {
                let _ = request.await;
            }));
        }
        changed
    }

    pub(crate) fn show_quick_action(&self, document: u64) -> bool {
        self.quick_actions.visible
            && self
                .quick_actions
                .pending
                .as_ref()
                .is_some_and(|(target, _)| target.document == document)
    }
}

pub(crate) fn button(
    editor: &Entity<EditorState>,
    buffer: &Rc<Buffer>,
    color: Hsla,
    hover: Hsla,
) -> impl IntoElement {
    deferred(QuickActionButton {
        editor: editor.downgrade(),
        buffer: Rc::downgrade(buffer),
        color,
        hover,
    })
    .with_priority(base::POPUP_PRIORITY - 1)
}

struct QuickActionButton {
    editor: WeakEntity<EditorState>,
    buffer: std::rc::Weak<Buffer>,
    color: Hsla,
    hover: Hsla,
}

impl Element for QuickActionButton {
    type RequestLayoutState = ();
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (window.request_layout(Style::default(), [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let editor = self.editor.upgrade()?;
        let state = editor.read(cx);
        let buffer = self.buffer.upgrade()?;
        if !state.focus_handle(cx).is_focused(window)
            || !buffer.has_code_actions(&action_range(
                &buffer,
                state.text(),
                state.selected_range(),
            ))
        {
            return None;
        }
        let geometry = state.prepaint_caret_geometry()?;
        let input = geometry.input_bounds();
        let row = geometry.cursor_bounds().top() + geometry.scroll_offset().y;
        let origin = point(
            input.left() - GUTTER_WIDTH + px(1.),
            row + (geometry.line_height() - BUTTON_SIZE) / 2.,
        );
        if origin.y < input.top() || origin.y + BUTTON_SIZE > input.bottom() {
            return None;
        }
        let hover = self.hover;
        let mut button = div()
            .id("editor-quick-actions")
            .role(Role::Button)
            .aria_label("Code Actions")
            .occlude()
            .size(BUTTON_SIZE)
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(ToggleCodeActions), cx);
                cx.stop_propagation();
            })
            .child(
                Icon::empty()
                    .path(ICON_PATH)
                    .size(px(13.))
                    .text_color(self.color),
            )
            .into_any_element();
        button.prepaint_as_root(origin, size(BUTTON_SIZE, BUTTON_SIZE).into(), window, cx);
        Some(button)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        button: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(button) = button {
            button.paint(window, cx);
        }
    }
}
