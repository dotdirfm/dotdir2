use crate::app::{DotDir, Modal};
use gpui_kit::{component::kbd::Kbd, *};
use std::time::Duration;

const DISPLAY_TIME: Duration = Duration::from_millis(1500);

#[derive(Default)]
pub(crate) struct PresentationKeystrokes {
    pub enabled: bool,
    label: Option<SharedString>,
    hide: Option<Task<()>>,
}

impl PresentationKeystrokes {
    pub fn clear(&mut self) -> bool {
        self.hide = None;
        self.label.take().is_some()
    }
}

impl DotDir {
    pub(crate) fn observe_presentation_keystrokes(
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let handle = window.window_handle();
        let view = cx.weak_entity();
        // Capture actions and raw keyboard input before either can consume the event.
        cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle() == handle && window.is_window_active() {
                let _ = view.update(cx, |this, cx| this.show_presentation_keystroke(event, cx));
            }
        })
    }

    fn shows_presentation_keystrokes(&self) -> bool {
        self.keystrokes.enabled
            && !matches!(
                self.modal,
                Some(Modal::ArchivePassword { .. } | Modal::CreateArchive(_))
            )
    }

    fn show_presentation_keystroke(&mut self, event: &KeystrokeEvent, cx: &mut Context<Self>) {
        if !self.shows_presentation_keystrokes() {
            return;
        }
        let Some(label) = presentation_label(&event.keystroke, event.input_preference) else {
            return;
        };
        self.keystrokes.label = Some(label.into());
        self.keystrokes.hide = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DISPLAY_TIME).await;
            let _ = this.update(cx, |this, cx| {
                this.keystrokes.label = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn render_presentation_keystroke(&self, window: &Window) -> Option<AnyElement> {
        if !window.is_window_active() || !self.shows_presentation_keystrokes() {
            return None;
        }
        let label = self.keystrokes.label.clone()?;
        let palette = self.palette;
        Some(
            deferred(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(px(72.))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .id("presentation-keystroke")
                            .role(Role::Label)
                            .aria_label(format!("Keystroke {label}"))
                            .min_w(px(56.))
                            .px(px(18.))
                            .py(px(7.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(palette.accent)
                            .bg(palette.bg)
                            .shadow_md()
                            .text_center()
                            .text_color(palette.fg)
                            .text_size(px(22.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(label),
                    ),
            )
            .with_priority(base::POPUP_PRIORITY + 1)
            .into_any_element(),
        )
    }
}

fn presentation_label(stroke: &Keystroke, input_preference: InputPreference) -> Option<String> {
    let key = stroke.key.as_str();
    if key.is_empty() || input_preference == InputPreference::CharacterInput {
        return None;
    }
    let modifiers = stroke.modifiers;
    let shortcut =
        modifiers.control || modifiers.platform || (modifiers.alt && stroke.key_char.is_none());
    let navigation = matches!(
        key,
        "backspace"
            | "delete"
            | "down"
            | "end"
            | "enter"
            | "escape"
            | "home"
            | "insert"
            | "left"
            | "pagedown"
            | "pageup"
            | "right"
            | "space"
            | "tab"
            | "up"
    );
    let function_key = key
        .strip_prefix('f')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=35).contains(&number));
    if !(shortcut || navigation || function_key || stroke.is_modifier_key()) {
        return None;
    }

    let key = match key {
        "control" => "ctrl",
        "platform" => "cmd",
        "function" => "Fn",
        key => key,
    };
    Some(Kbd::format(&Keystroke {
        modifiers,
        key: key.into(),
        key_char: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::presentation_label;
    use gpui_kit::{InputPreference, Keystroke};

    #[test]
    fn shows_shortcuts_and_navigation_without_echoing_typed_characters() {
        let label = |binding, input_preference| {
            presentation_label(&Keystroke::parse(binding).unwrap(), input_preference)
        };
        assert_eq!(
            label("f4", InputPreference::KeyBindings).as_deref(),
            Some("F4")
        );
        for binding in ["down", "shift-tab", "cmd-f3", "ctrl-c", "space"] {
            assert!(
                label(binding, InputPreference::KeyBindings).is_some(),
                "{binding}"
            );
        }
        for binding in ["a", "shift-a", "5", "f36"] {
            assert_eq!(
                label(binding, InputPreference::KeyBindings),
                None,
                "{binding}"
            );
        }
        assert_eq!(label("ctrl-alt-q", InputPreference::CharacterInput), None);
    }
}
