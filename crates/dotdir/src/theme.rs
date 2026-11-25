use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Hsla, rgb};

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Hsla,
    pub fg: Hsla,
    pub secondary: Hsla,
    pub muted: Hsla,
    pub border: Hsla,
    pub accent: Hsla,
    pub accent_fg: Hsla,
    pub selected: Hsla,
    pub marked: Hsla,
    pub input: Hsla,
    pub folder: Hsla,
    pub error: Hsla,
}
impl Palette {
    pub fn named(name: &str) -> Self {
        let colors = match name {
            "light" => [
                0xeff1f5, 0x4c4f69, 0xe6e9ef, 0x8c8fa1, 0xccd0da, 0x1e66f5, 0xffffff, 0xccd0da,
                0xf1dfb4, 0xffffff, 0xdba31c, 0xd20f39,
            ],
            "mocha" => [
                0x1e1e2e, 0xcdd6f4, 0x181825, 0x6c7086, 0x313244, 0x89b4fa, 0x1e1e2e, 0x313244,
                0x403c3b, 0x181825, 0x89b4fa, 0xf38ba8,
            ],
            _ => [
                0x1f2430, 0xcac8be, 0x1f2430, 0x707e91, 0x171b24, 0xffcc66, 0x735923, 0x292f3e,
                0x3c3832, 0x242936, 0x48505b, 0xff6666,
            ],
        };
        let [
            bg,
            fg,
            secondary,
            muted,
            border,
            accent,
            accent_fg,
            selected,
            marked,
            input,
            folder,
            error,
        ] = colors.map(|v| rgb(v).into());
        Self {
            bg,
            fg,
            secondary,
            muted,
            border,
            accent,
            accent_fg,
            selected,
            marked,
            input,
            folder,
            error,
        }
    }
    pub fn apply(self, name: &str, cx: &mut App) {
        cx.set_window_appearance(Some(if name == "light" {
            gpui_kit::WindowAppearance::Light
        } else {
            gpui_kit::WindowAppearance::Dark
        }));
        Theme::change(
            if name == "light" {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            },
            None,
            cx,
        );
        let t = Theme::global_mut(cx);
        t.font_family = ".AppleSystemUIFont".into();
        t.colors.background = self.bg;
        t.colors.foreground = self.fg;
        t.colors.border = self.border;
        t.colors.input = self.input;
        t.colors.primary = self.accent;
        t.colors.primary_foreground = self.accent_fg;
        t.colors.accent = self.selected;
        t.colors.accent_foreground = self.fg;
        t.colors.muted = self.secondary;
        t.colors.muted_foreground = self.muted;
        t.colors.selection = self.selected;
    }
}
