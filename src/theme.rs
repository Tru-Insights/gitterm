#![allow(dead_code)]
use iced::{color, Color, Theme};

// App theme (affects entire UI)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AppTheme {
    #[default]
    Dark,
    Light,
}

impl AppTheme {
    pub fn toggle(&self) -> Self {
        match self {
            AppTheme::Dark => AppTheme::Light,
            AppTheme::Light => AppTheme::Dark,
        }
    }
}

// Theme color functions - complete Catppuccin palette for future use
// All color functions suppressed to avoid unused warnings
#[allow(dead_code)]
pub fn catppuccin_mocha_bg() -> Color {
    color!(0x1e1e2e)
}

pub fn catppuccin_mocha_surface0() -> Color {
    color!(0x313244)
}

pub fn catppuccin_mocha_surface1() -> Color {
    color!(0x45475a)
}

pub fn catppuccin_mocha_surface2() -> Color {
    color!(0x585b70)
}

pub fn catppuccin_mocha_text() -> Color {
    color!(0xcdd6f4)
}

pub fn catppuccin_mocha_subtext1() -> Color {
    color!(0xbac2de)
}

pub fn catppuccin_mocha_subtext0() -> Color {
    color!(0xa6adc8)
}

pub fn catppuccin_mocha_overlay2() -> Color {
    color!(0x9399b2)
}

pub fn catppuccin_mocha_overlay1() -> Color {
    color!(0x7f849c)
}

pub fn catppuccin_mocha_overlay0() -> Color {
    color!(0x6c7086)
}

pub fn catppuccin_mocha_blue() -> Color {
    color!(0x89b4fa)
}

pub fn catppuccin_mocha_lavender() -> Color {
    color!(0xb4befe)
}

pub fn catppuccin_mocha_sapphire() -> Color {
    color!(0x74c7ec)
}

pub fn catppuccin_mocha_sky() -> Color {
    color!(0x89dceb)
}

pub fn catppuccin_mocha_teal() -> Color {
    color!(0x94e2d5)
}

pub fn catppuccin_mocha_green() -> Color {
    color!(0xa6e3a1)
}

pub fn catppuccin_mocha_yellow() -> Color {
    color!(0xf9e2af)
}

pub fn catppuccin_mocha_peach() -> Color {
    color!(0xfab387)
}

pub fn catppuccin_mocha_maroon() -> Color {
    color!(0xeba0ac)
}

pub fn catppuccin_mocha_red() -> Color {
    color!(0xf38ba8)
}

pub fn catppuccin_mocha_mauve() -> Color {
    color!(0xcba6f7)
}

pub fn catppuccin_mocha_pink() -> Color {
    color!(0xf5c2e7)
}

pub fn catppuccin_mocha_flamingo() -> Color {
    color!(0xf2cdcd)
}

pub fn catppuccin_mocha_rosewater() -> Color {
    color!(0xf5e0dc)
}

// Latte colors (light theme)
pub fn catppuccin_latte_base() -> Color {
    color!(0xeff1f5)
}

pub fn catppuccin_latte_mantle() -> Color {
    color!(0xe6e9ef)
}

pub fn catppuccin_latte_crust() -> Color {
    color!(0xdce0e8)
}

pub fn catppuccin_latte_surface0() -> Color {
    color!(0xccd0da)
}

pub fn catppuccin_latte_surface1() -> Color {
    color!(0xbcc0cc)
}

pub fn catppuccin_latte_surface2() -> Color {
    color!(0xacb0be)
}

pub fn catppuccin_latte_text() -> Color {
    color!(0x4c4f69)
}

pub fn catppuccin_latte_subtext1() -> Color {
    color!(0x5c5f77)
}

pub fn catppuccin_latte_subtext0() -> Color {
    color!(0x6c6f85)
}

pub fn catppuccin_latte_overlay2() -> Color {
    color!(0x7c7f93)
}

pub fn catppuccin_latte_overlay1() -> Color {
    color!(0x8c8fa1)
}

pub fn catppuccin_latte_overlay0() -> Color {
    color!(0x9ca0b0)
}

pub fn catppuccin_latte_blue() -> Color {
    color!(0x1e66f5)
}

pub fn catppuccin_latte_lavender() -> Color {
    color!(0x7287fd)
}

pub fn catppuccin_latte_sapphire() -> Color {
    color!(0x209fb5)
}

pub fn catppuccin_latte_sky() -> Color {
    color!(0x04a5e5)
}

pub fn catppuccin_latte_teal() -> Color {
    color!(0x179299)
}

pub fn catppuccin_latte_green() -> Color {
    color!(0x40a02b)
}

pub fn catppuccin_latte_yellow() -> Color {
    color!(0xdf8e1d)
}

pub fn catppuccin_latte_peach() -> Color {
    color!(0xfe640b)
}

pub fn catppuccin_latte_maroon() -> Color {
    color!(0xe64553)
}

pub fn catppuccin_latte_red() -> Color {
    color!(0xd20f39)
}

pub fn catppuccin_latte_mauve() -> Color {
    color!(0x8839ef)
}

pub fn catppuccin_latte_pink() -> Color {
    color!(0xea76cb)
}

pub fn catppuccin_latte_flamingo() -> Color {
    color!(0xdd7878)
}

pub fn catppuccin_latte_rosewater() -> Color {
    color!(0xdc8a78)
}

impl AppTheme {
    // Background colors
    #[allow(dead_code)]
    pub fn bg(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_bg(),
            AppTheme::Light => catppuccin_latte_base(),
        }
    }

    pub fn surface0(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_surface0(),
            AppTheme::Light => catppuccin_latte_surface0(),
        }
    }

    pub fn surface1(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_surface1(),
            AppTheme::Light => catppuccin_latte_surface1(),
        }
    }

    pub fn surface2(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_surface2(),
            AppTheme::Light => catppuccin_latte_surface2(),
        }
    }

    // Text colors
    #[allow(dead_code)]
    pub fn text(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_text(),
            AppTheme::Light => catppuccin_latte_text(),
        }
    }

    pub fn subtext1(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_subtext1(),
            AppTheme::Light => catppuccin_latte_subtext1(),
        }
    }

    pub fn subtext0(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_subtext0(),
            AppTheme::Light => catppuccin_latte_subtext0(),
        }
    }

    // Accent colors
    pub fn blue(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_blue(),
            AppTheme::Light => catppuccin_latte_blue(),
        }
    }

    pub fn green(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_green(),
            AppTheme::Light => catppuccin_latte_green(),
        }
    }

    #[allow(dead_code)]
    pub fn red(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_red(),
            AppTheme::Light => catppuccin_latte_red(),
        }
    }

    pub fn yellow(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_yellow(),
            AppTheme::Light => catppuccin_latte_yellow(),
        }
    }

    pub fn mauve(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_mauve(),
            AppTheme::Light => catppuccin_latte_mauve(),
        }
    }

    #[allow(dead_code)]
    pub fn pink(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_pink(),
            AppTheme::Light => catppuccin_latte_pink(),
        }
    }

    pub fn peach(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_peach(),
            AppTheme::Light => catppuccin_latte_peach(),
        }
    }

    pub fn overlay0(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_overlay0(),
            AppTheme::Light => catppuccin_latte_overlay0(),
        }
    }

    pub fn overlay1(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_overlay1(),
            AppTheme::Light => catppuccin_latte_overlay1(),
        }
    }

    #[allow(dead_code)]
    pub fn overlay2(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_overlay2(),
            AppTheme::Light => catppuccin_latte_overlay2(),
        }
    }

    // Convert to Iced theme
    #[allow(dead_code)]
    pub fn iced_theme(&self) -> Theme {
        match self {
            AppTheme::Dark => Theme::Dark,
            AppTheme::Light => Theme::Light,
        }
    }

    // Terminal color palette
    pub fn terminal_palette(&self) -> iced_term::ColorPalette {
        match self {
            AppTheme::Dark => iced_term::ColorPalette {
                // Catppuccin Mocha
                background: String::from("#1e1e2e"),
                foreground: String::from("#cdd6f4"),
                black: String::from("#45475a"),
                red: String::from("#f38ba8"),
                green: String::from("#a6e3a1"),
                yellow: String::from("#f9e2af"),
                blue: String::from("#89b4fa"),
                magenta: String::from("#f5c2e7"),
                cyan: String::from("#94e2d5"),
                white: String::from("#bac2de"),
                bright_black: String::from("#585b70"),
                bright_red: String::from("#f38ba8"),
                bright_green: String::from("#a6e3a1"),
                bright_yellow: String::from("#f9e2af"),
                bright_blue: String::from("#89b4fa"),
                bright_magenta: String::from("#f5c2e7"),
                bright_cyan: String::from("#94e2d5"),
                bright_white: String::from("#a6adc8"),
                bright_foreground: Some(String::from("#cdd6f4")),
                dim_foreground: String::from("#7f849c"),
                dim_black: String::from("#313244"),
                dim_red: String::from("#a65d6d"),
                dim_green: String::from("#6e9a6d"),
                dim_yellow: String::from("#a69a74"),
                dim_blue: String::from("#5d78a6"),
                dim_magenta: String::from("#a6849c"),
                dim_cyan: String::from("#649a92"),
                dim_white: String::from("#7f849c"),
            },
            AppTheme::Light => iced_term::ColorPalette {
                // Catppuccin Latte
                background: String::from("#eff1f5"),
                foreground: String::from("#4c4f69"),
                black: String::from("#5c5f77"),
                red: String::from("#d20f39"),
                green: String::from("#40a02b"),
                yellow: String::from("#df8e1d"),
                blue: String::from("#1e66f5"),
                magenta: String::from("#ea76cb"),
                cyan: String::from("#179299"),
                white: String::from("#acb0be"),
                bright_black: String::from("#6c6f85"),
                bright_red: String::from("#d20f39"),
                bright_green: String::from("#40a02b"),
                bright_yellow: String::from("#df8e1d"),
                bright_blue: String::from("#1e66f5"),
                bright_magenta: String::from("#ea76cb"),
                bright_cyan: String::from("#179299"),
                bright_white: String::from("#bcc0cc"),
                bright_foreground: Some(String::from("#4c4f69")),
                dim_foreground: String::from("#6c6f85"),
                dim_black: String::from("#4c4f69"),
                dim_red: String::from("#a10c2d"),
                dim_green: String::from("#338022"),
                dim_yellow: String::from("#b27117"),
                dim_blue: String::from("#1852c4"),
                dim_magenta: String::from("#bb5ea2"),
                dim_cyan: String::from("#12747a"),
                dim_white: String::from("#8c8fa1"),
            },
        }
    }

    // UI Colors
    pub fn bg_base(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x1e1e2e),
            AppTheme::Light => color!(0xeff1f5),
        }
    }

    pub fn bg_surface(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x181825),
            AppTheme::Light => color!(0xe6e9ef),
        }
    }

    pub fn bg_overlay(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x313244),
            AppTheme::Light => color!(0xdce0e8),
        }
    }

    pub fn text_primary(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0xcdd6f4),
            AppTheme::Light => color!(0x4c4f69),
        }
    }

    pub fn text_secondary(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x6c7086),
            AppTheme::Light => color!(0x8c8fa1),
        }
    }

    pub fn text_muted(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x45475a),
            AppTheme::Light => color!(0xbcc0cc),
        }
    }

    pub fn accent(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x89b4fa),
            AppTheme::Light => color!(0x1e66f5),
        }
    }

    pub fn border(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x45475a),
            AppTheme::Light => color!(0xccd0da),
        }
    }

    pub fn success(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0xa6e3a1),
            AppTheme::Light => color!(0x40a02b),
        }
    }

    pub fn warning(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0xf9e2af),
            AppTheme::Light => color!(0xdf8e1d),
        }
    }

    pub fn danger(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0xf38ba8),
            AppTheme::Light => color!(0xd20f39),
        }
    }

    pub fn diff_add_bg(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x1a3a1a),
            AppTheme::Light => color!(0xd4f4d4),
        }
    }

    pub fn diff_del_bg(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x3a1a1a),
            AppTheme::Light => color!(0xf4d4d4),
        }
    }

    pub fn diff_add_highlight(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x3a6b3a),
            AppTheme::Light => color!(0x90d090),
        }
    }

    pub fn diff_del_highlight(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x6b3a3a),
            AppTheme::Light => color!(0xd09090),
        }
    }

    pub fn bg_crust(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x11111b),
            AppTheme::Light => color!(0xdce0e8),
        }
    }
}
