use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Hsla, rgb};

#[derive(Clone, Copy)]
pub struct Palette {
    pub mode: ThemeMode,
    pub bg: Hsla,
    pub fg: Hsla,
    pub secondary: Hsla,
    pub muted: Hsla,
    pub border: Hsla,
    pub accent: Hsla,
    pub accent_fg: Hsla,
    pub selected: Hsla,
    pub selected_fg: Option<Hsla>,
    pub marked: Hsla,
    pub input: Hsla,
    pub command_bg: Hsla,
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
            mode: if name == "light" {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            },
            bg,
            fg,
            secondary,
            muted,
            border,
            accent,
            accent_fg,
            selected,
            selected_fg: None,
            marked,
            input,
            command_bg: if name == "light" {
                input
            } else {
                rgb(0x171b24).into()
            },
            folder,
            error,
        }
    }
    pub fn apply(self, cx: &mut App) {
        cx.set_window_appearance(Some(if self.mode.is_dark() {
            gpui_kit::WindowAppearance::Dark
        } else {
            gpui_kit::WindowAppearance::Light
        }));
        Theme::change(self.mode, None, cx);
        Theme::update(cx, |t| {
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
        });
    }
}
