use gpui::*;
use gpui_component::{Theme, ThemeMode};

// origin/v6 src/config.rs defaults and src/theme.rs Catppuccin palettes.
pub const UI_FONT_SIZE: f32 = 13.;
pub const TERMINAL_FONT_SIZE: f32 = 14.;
#[derive(Clone, Copy)]
pub struct Palette {
    pub base: u32,
    pub mantle: u32,
    pub surface: u32,
    pub text: u32,
    pub muted: u32,
    pub blue: u32,
    pub green: u32,
    pub red: u32,
}
impl Palette {
    pub fn current(cx: &App) -> Self {
        Self::for_mode(Theme::global(cx).mode.is_dark())
    }
    pub fn for_mode(dark: bool) -> Self {
        if dark {
            Self {
                base: 0x1e1e2e,
                mantle: 0x181825,
                surface: 0x313244,
                text: 0xcdd6f4,
                muted: 0xa6adc8,
                blue: 0x89b4fa,
                green: 0xa6e3a1,
                red: 0xf38ba8,
            }
        } else {
            Self {
                base: 0xeff1f5,
                mantle: 0xe6e9ef,
                surface: 0xccd0da,
                text: 0x4c4f69,
                muted: 0x6c6f85,
                blue: 0x1e66f5,
                green: 0x40a02b,
                red: 0xd20f39,
            }
        }
    }
}
pub fn apply(dark: bool, window: Option<&mut Window>, cx: &mut App) {
    Theme::change(
        if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        window,
        cx,
    );
    let p = Palette::for_mode(dark);
    Theme::update(cx, |t| {
        t.font_family = ".SystemUIFont".into();
        t.font_size = px(UI_FONT_SIZE);
        t.mono_font_family = "Menlo".into();
        t.mono_font_size = px(TERMINAL_FONT_SIZE);
        t.background = rgb(p.base).into();
        t.foreground = rgb(p.text).into();
        t.border = rgb(p.surface).into();
        t.input = rgb(p.surface).into();
        t.muted = rgb(p.surface).into();
        t.muted_foreground = rgb(p.muted).into();
        t.primary = rgb(p.blue).into();
        t.primary_foreground = rgb(p.base).into();
        t.accent = rgb(p.surface).into();
        t.accent_foreground = rgb(p.text).into();
        t.secondary = rgb(p.surface).into();
        t.secondary_foreground = rgb(p.text).into();
        t.popover = rgb(p.base).into();
        t.popover_foreground = rgb(p.text).into();
        t.tab = rgb(p.mantle).into();
        t.tab_bar = rgb(p.mantle).into();
        t.tab_active = rgb(p.base).into();
        t.tab_foreground = rgb(p.muted).into();
        t.tab_active_foreground = rgb(p.text).into();
        t.title_bar = rgb(p.mantle).into();
        t.title_bar_border = rgb(p.surface).into();
        t.sidebar = rgb(p.mantle).into();
        t.sidebar_foreground = rgb(p.text).into();
        t.sidebar_border = rgb(p.surface).into();
        t.sidebar_accent = rgb(p.surface).into();
        t.colors.list = rgb(p.base).into();
        t.list_hover = rgb(p.surface).into();
        t.list_active = rgb(p.surface).into();
        t.selection = rgb(p.surface).into();
        t.ring = rgb(p.blue).into();
        t.success = rgb(p.green).into();
        t.danger = rgb(p.red).into();
    });
}
