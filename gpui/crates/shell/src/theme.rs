use gpui::{App, Window, px, rgb};
use gpui_component::{Theme, ThemeMode};

// Iced src/main.rs label typography and core settings defaults.
pub const LABEL_FONT: &str = "Menlo";
pub const LABEL_LINE_HEIGHT: f32 = 1.3;
pub const UI_FONT_SIZE: f32 = 13.;
pub const TERMINAL_FONT_SIZE: f32 = 14.;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub base: u32,
    pub mantle: u32,
    pub crust: u32,
    /// Iced bg_overlay differs from surface0 in Latte.
    pub overlay: u32,
    pub border: u32,
    pub surface: u32,
    pub surface2: u32,
    pub text: u32,
    pub subtext0: u32,
    pub muted: u32,
    pub overlay0: u32,
    pub peach: u32,
    pub yellow: u32,
    pub lavender: u32,
    pub pink: u32,
    pub teal: u32,
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
                crust: 0x11111b,
                overlay: 0x313244,
                border: 0x45475a,
                surface: 0x313244,
                surface2: 0x585b70,
                text: 0xcdd6f4,
                subtext0: 0xa6adc8,
                muted: 0x9399b2,
                overlay0: 0x6c7086,
                peach: 0xfab387,
                yellow: 0xf9e2af,
                lavender: 0xb4befe,
                pink: 0xf5c2e7,
                teal: 0x94e2d5,
                blue: 0x89b4fa,
                green: 0xa6e3a1,
                red: 0xf38ba8,
            }
        } else {
            Self {
                base: 0xeff1f5,
                mantle: 0xe6e9ef,
                crust: 0xdce0e8,
                overlay: 0xdce0e8,
                border: 0xccd0da,
                surface: 0xccd0da,
                surface2: 0xacb0be,
                text: 0x4c4f69,
                subtext0: 0x6c6f85,
                muted: 0x7c7f93,
                overlay0: 0x9ca0b0,
                peach: 0xfe640b,
                yellow: 0xdf8e1d,
                lavender: 0x7287fd,
                pink: 0xea76cb,
                teal: 0x179299,
                blue: 0x1e66f5,
                green: 0x40a02b,
                red: 0xd20f39,
            }
        }
    }
}
/// Apply persisted font/theme settings without reading or writing a config root.
/// The host supplies the settings from its existing settings owner.
pub fn apply_settings<C, U, R>(
    settings: &gitterm_core::settings::Settings<C, U, R>,
    window: Option<&mut Window>,
    cx: &mut App,
) {
    // Iced selects Light only for the exact persisted value "light".
    apply(settings.theme != "light", window, cx);
    let (terminal, ui) = settings.font_size.map_or(
        (settings.terminal_font_size, settings.ui_font_size),
        |legacy| (legacy, legacy - 1.),
    );
    Theme::update(cx, |t| {
        t.font_size = px(ui.clamp(10., 24.));
        t.mono_font_size = px(terminal.clamp(10., 24.));
    });
}

pub fn sync_terminal_typography(view: &gpui::Entity<gitterm_terminal::TerminalView>, cx: &mut App) {
    let theme = Theme::global(cx);
    let current = view.read(cx).config();
    if current.font_family == theme.mono_font_family && current.font_size == theme.mono_font_size {
        return;
    }
    let mut config = current.clone();
    config.font_family = theme.mono_font_family.to_string();
    config.font_size = theme.mono_font_size;
    view.update(cx, |view, cx| view.update_config(config, cx));
}

pub fn apply(dark: bool, window: Option<&mut Window>, cx: &mut App) {
    // A mode toggle must retain the owner's configured font sizes.
    let previous = Theme::global(cx);
    let (ui_size, terminal_size) = if previous.font_family == LABEL_FONT {
        (previous.font_size, previous.mono_font_size)
    } else {
        (px(UI_FONT_SIZE), px(TERMINAL_FONT_SIZE))
    };
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
        t.font_family = LABEL_FONT.into();
        t.font_size = ui_size;
        t.mono_font_family = LABEL_FONT.into();
        t.mono_font_size = terminal_size;
        t.background = rgb(p.base).into();
        t.foreground = rgb(p.text).into();
        t.border = rgb(p.border).into();
        t.input = rgb(p.border).into();
        t.muted = rgb(p.surface).into();
        t.muted_foreground = rgb(p.muted).into();
        t.primary = rgb(p.blue).into();
        t.primary_foreground = rgb(p.base).into();
        t.primary_hover = rgb(p.blue).into();
        t.primary_active = rgb(p.blue).into();
        t.button = rgb(p.mantle).into();
        t.button_foreground = rgb(p.text).into();
        t.button_hover = rgb(p.overlay).into();
        t.button_active = rgb(p.overlay).into();
        t.button_primary = rgb(p.blue).into();
        t.button_primary_foreground = rgb(p.base).into();
        t.button_primary_hover = rgb(p.blue).into();
        t.button_primary_active = rgb(p.blue).into();
        t.button_secondary = rgb(p.overlay).into();
        t.button_secondary_foreground = rgb(p.text).into();
        t.button_secondary_hover = rgb(p.surface).into();
        t.button_secondary_active = rgb(p.surface).into();
        t.accent = rgb(p.overlay).into();
        t.accent_foreground = rgb(p.text).into();
        t.secondary = rgb(p.overlay).into();
        t.secondary_foreground = rgb(p.text).into();
        t.popover = rgb(p.base).into();
        t.popover_foreground = rgb(p.text).into();
        t.tab = rgb(p.mantle).into();
        t.tab_bar = rgb(p.mantle).into();
        t.tab_bar_segmented = rgb(p.mantle).into();
        t.tab_active = rgb(p.base).into();
        t.tab_foreground = rgb(p.subtext0).into();
        t.tab_active_foreground = rgb(p.text).into();
        t.title_bar = rgb(p.mantle).into();
        t.title_bar_border = rgb(p.border).into();
        t.sidebar = rgb(p.mantle).into();
        t.sidebar_foreground = rgb(p.text).into();
        t.sidebar_border = rgb(p.border).into();
        t.sidebar_accent = rgb(p.overlay).into();
        t.colors.list = rgb(p.base).into();
        t.list_hover = rgb(p.overlay).into();
        t.list_active = rgb(p.overlay).into();
        t.selection = rgb(p.surface).into();
        t.sidebar_accent_foreground = rgb(p.text).into();
        t.sidebar_primary = rgb(p.blue).into();
        t.sidebar_primary_foreground = rgb(p.base).into();
        t.status_bar = rgb(p.crust).into();
        t.status_bar_border = rgb(p.surface).into();
        t.window_border = rgb(p.border).into();
        t.ring = rgb(p.blue).into();
        t.link = rgb(p.blue).into();
        t.link_hover = rgb(p.blue).into();
        t.link_active = rgb(p.blue).into();
        t.list_active_border = rgb(p.blue).into();
        t.info = rgb(p.blue).into();
        t.caret = rgb(p.text).into();
        t.success = rgb(p.green).into();
        t.warning = rgb(p.yellow).into();
        t.danger = rgb(p.red).into();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::FixtureSettings;
    use gpui::{AppContext as _, Hsla, TestAppContext};

    #[gpui::test]
    fn terminal_view_uses_shared_font_settings(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let view = cx.new(|cx| {
            gitterm_terminal::TerminalView::new_deferred(
                gitterm_terminal::BackendSettings::default(),
                gitterm_terminal::TerminalConfig::default(),
                cx,
            )
        });
        cx.update(|cx| {
            let settings = FixtureSettings {
                terminal_font_size: 19.,
                ..Default::default()
            };
            apply_settings(&settings, None, cx);
            sync_terminal_typography(&view, cx);
            sync_terminal_typography(&view, cx);
            // Headless GPUI has no installed fonts and resolves Menlo to its fallback.
            assert_eq!(
                view.read(cx).config().font_family,
                Theme::global(cx).mono_font_family
            );
            assert_eq!(view.read(cx).config().font_size, px(19.));
            assert_eq!(view.read(cx).config().scrollback, 100_000);
        });
    }

    #[gpui::test]
    fn startup_settings_fixture_applies_light_and_missing_uses_defaults(cx: &mut TestAppContext) {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(
            fixture.path().join("settings.json"),
            br#"{"theme":"light","ui_font_size":17,"terminal_font_size":19}"#,
        )
        .unwrap();
        let settings = crate::workspace_config::load_iced_settings_from(fixture.path());
        let defaults =
            crate::workspace_config::load_iced_settings_from(&fixture.path().join("absent"));
        cx.update(gpui_component::init);
        cx.update(|cx| {
            apply_settings(&settings, None, cx);
            let theme = Theme::global(cx);
            assert_eq!(theme.mode, ThemeMode::Light);
            assert_eq!(theme.font_family, LABEL_FONT);
            assert_eq!(theme.font_size, px(17.));
            assert_eq!(theme.mono_font_size, px(19.));
            apply_settings(&defaults, None, cx);
            let theme = Theme::global(cx);
            assert_eq!(theme.mode, ThemeMode::Dark);
            assert_eq!(theme.font_size, px(UI_FONT_SIZE));
            assert_eq!(theme.mono_font_size, px(TERMINAL_FONT_SIZE));
        });
    }

    // Read the authoritative constants so a future Iced palette change cannot
    // leave the GPUI expectations silently pinned to an obsolete copy.
    fn iced_colors(function: &str) -> Vec<u32> {
        let source = include_str!("../../../../src/theme.rs");
        let body = source
            .split_once(&format!("pub fn {function}("))
            .unwrap()
            .1
            .split("\n    pub fn ")
            .next()
            .unwrap()
            .split("\npub fn ")
            .next()
            .unwrap();
        body.split("color!(0x")
            .skip(1)
            .map(|value| u32::from_str_radix(value.split(')').next().unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn palettes_match_iced_constants_in_both_modes() {
        for (dark, index, name) in [(true, 0, "mocha"), (false, 1, "latte")] {
            let p = Palette::for_mode(dark);
            for (function, actual) in [
                ("bg_base", p.base),
                ("bg_surface", p.mantle),
                ("bg_crust", p.crust),
                ("bg_overlay", p.overlay),
                ("border", p.border),
                ("text_primary", p.text),
                ("text_secondary", p.subtext0),
                ("text_muted", p.muted),
                ("accent", p.blue),
                ("success", p.green),
                ("warning", p.yellow),
                ("danger", p.red),
            ] {
                assert_eq!(actual, iced_colors(function)[index], "{name} {function}");
            }
            for (suffix, actual) in [
                ("surface0", p.surface),
                ("surface2", p.surface2),
                ("overlay0", p.overlay0),
                ("peach", p.peach),
                ("lavender", p.lavender),
                ("pink", p.pink),
                ("teal", p.teal),
            ] {
                let function = format!("catppuccin_{name}_{suffix}");
                assert_eq!(actual, iced_colors(&function)[0], "{function}");
            }
            let strip = crate::workspace_bar::Colors::new(dark);
            assert_eq!(strip.base, p.base);
            assert_eq!(strip.surface0, p.surface);
            assert_eq!(strip.subtext0, p.subtext0);
            assert_eq!(strip.muted, p.muted);
        }
    }

    #[gpui::test]
    fn global_theme_matches_iced_and_applies_settings(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(|cx| {
            for mode in ["dark", "light", "unknown", "Light"] {
                let settings = FixtureSettings {
                    theme: mode.into(),
                    ui_font_size: 17.,
                    terminal_font_size: 19.,
                    ..Default::default()
                };
                apply_settings(&settings, None, cx);
                let dark = mode != "light";
                let p = Palette::for_mode(dark);
                let t = Theme::global(cx);
                assert_eq!(t.mode.is_dark(), dark);
                assert_eq!(t.font_family, "Menlo");
                assert_eq!(t.font_size, px(17.));
                assert_eq!(t.mono_font_size, px(19.));
                for (actual, expected) in [
                    (t.background, p.base),
                    (t.foreground, p.text),
                    (t.border, p.border),
                    (t.input, p.border),
                    (t.muted_foreground, p.muted),
                    (t.primary, p.blue),
                    (t.button, p.mantle),
                    (t.button_hover, p.overlay),
                    (t.button_primary, p.blue),
                    (t.button_secondary, p.overlay),
                    (t.accent, p.overlay),
                    (t.secondary, p.overlay),
                    (t.tab_bar, p.mantle),
                    (t.tab_foreground, p.subtext0),
                    (t.sidebar, p.mantle),
                    (t.sidebar_border, p.border),
                    (t.status_bar, p.crust),
                    (t.success, p.green),
                    (t.warning, p.yellow),
                    (t.danger, p.red),
                ] {
                    assert_eq!(actual, Hsla::from(rgb(expected)));
                }
                assert_eq!(t.tokens.button_primary.color, t.primary);
                assert_eq!(t.typography_tokens().sans, "Menlo");
                assert_eq!(t.typography_tokens().md.size, px(17.));
                apply(!dark, None, cx);
                assert_eq!(Theme::global(cx).font_size, px(17.));
                assert_eq!(Theme::global(cx).mono_font_size, px(19.));
            }
            apply_settings(&FixtureSettings::default(), None, cx);
            assert_eq!(Theme::global(cx).font_size, px(UI_FONT_SIZE));
            assert_eq!(Theme::global(cx).mono_font_size, px(TERMINAL_FONT_SIZE));
            for (legacy, ui, terminal) in [(16., 15., 16.), (1., 10., 10.), (30., 24., 24.)] {
                let settings = FixtureSettings {
                    font_size: Some(legacy),
                    ui_font_size: 20.,
                    ..Default::default()
                };
                apply_settings(&settings, None, cx);
                assert_eq!(Theme::global(cx).font_size, px(ui));
                assert_eq!(Theme::global(cx).mono_font_size, px(terminal));
            }
        });
        assert_eq!(LABEL_LINE_HEIGHT, 1.3);
    }
}
